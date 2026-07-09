//! Tier-3 language packs (issue #78, Phase 2c part 3): Ruby, PHP, Kotlin, Swift,
//! Scala, C#.
//!
//! Unlike the tier-1 languages — whose definition extraction is a flat
//! tree-sitter query plus per-language `match` arms in [`crate::parse`] — the
//! tier-3 packs share one **config-driven recursive-descent extractor** here.
//! This mirrors how CodeGraph 0.9.7 itself extracts (a single `TreeSitterExtractor`
//! walker driven by per-language `LanguageExtractor` config objects), which is
//! what lets us reach node/qualified-name/edge parity with its schema-v4 output
//! for these six languages — several of which need behaviours the tier-1 query
//! path does not model: JVM package → `namespace` scoping (Kotlin), `module`
//! nesting (Ruby), `trait`/`interface`/`property`/`field`/`constant` node kinds,
//! and receiver-typed extension methods.
//!
//! Each language contributes a self-contained module ([`ruby`], [`php`],
//! [`kotlin`], [`swift`], [`scala`], [`csharp`]) exposing a [`LangSpec`] — the
//! node-type sets plus small hook functions the walker calls. A per-language
//! `.scm` file in `src/queries/<lang>.scm` documents the captured definition
//! node types (the same sets the [`LangSpec`] carries) for parity with the
//! tier-1 layout and to keep the capture vocabulary reviewable in one place.
//!
//! The extractor produces the same [`crate::model`] records and reference
//! [`crate::resolve`] sites the tier-1 path does, so the shared Phase-2b
//! resolver, writer, and incremental sync all work unchanged.
//!
//! Definition-extraction conventions are adapted from CodeGraph's MIT-licensed
//! extractor configs and engine (<https://github.com/colbymchenry/codegraph>);
//! see the repository `NOTICE`.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::model::{content_hash, EdgeRecord, FileRecord, Language, NodeRecord};
use crate::parse::FileExtract;
use crate::resolve::{RawSite, SitePayload};

mod csharp;
mod kotlin;
mod php;
mod ruby;
mod scala;
mod swift;

/// A borrowed tree-sitter node hook returning an optional owned string.
type StrHook = for<'a> fn(Node<'a>, &str) -> Option<String>;
/// A predicate hook over a node (with source for text checks).
type BoolHook = for<'a> fn(Node<'a>, &str) -> bool;
/// An import-extraction hook: returns `(module_name, signature)`.
type ImportHook = for<'a> fn(Node<'a>, &str) -> Option<(String, String)>;

/// Per-language configuration consumed by the shared [`Extractor`] walker.
///
/// The `*_types` fields are tree-sitter node-type names classifying each
/// definition; the hooks derive names/signatures/visibility/etc. that vary by
/// language. A `None` hook means "not applicable" (the walker uses a default).
pub(crate) struct LangSpec {
    pub language: Language,
    pub grammar: fn() -> tree_sitter::Language,

    pub function_types: &'static [&'static str],
    pub method_types: &'static [&'static str],
    pub class_types: &'static [&'static str],
    pub interface_types: &'static [&'static str],
    pub struct_types: &'static [&'static str],
    pub enum_types: &'static [&'static str],
    pub enum_member_types: &'static [&'static str],
    pub type_alias_types: &'static [&'static str],
    pub import_types: &'static [&'static str],
    pub call_types: &'static [&'static str],
    pub instantiation_types: &'static [&'static str],
    pub variable_types: &'static [&'static str],
    pub field_types: &'static [&'static str],
    pub property_types: &'static [&'static str],
    pub extra_class_node_types: &'static [&'static str],
    pub package_types: &'static [&'static str],

    /// Node kind emitted for [`Self::interface_types`] (`"interface"` or
    /// `"trait"`).
    pub interface_kind: &'static str,
    /// tree-sitter field name carrying a definition's identifier.
    pub name_field: &'static str,
    /// tree-sitter field name carrying a definition's body (fallback when
    /// [`Self::resolve_body`] is `None`).
    pub body_field: &'static str,

    /// Override a definition's name (e.g. Kotlin extension functions, whose name
    /// is the identifier *after* the receiver). `None` uses the default
    /// name-field/first-identifier scan.
    pub resolve_name: Option<StrHook>,
    /// Reclassify a `class_types` node into `class`/`struct`/`enum`/`interface`/
    /// `trait` (languages that reuse one node type for several kinds).
    pub classify_class: Option<StrHook>,
    /// The function/method `signature` column value (params + return type), or
    /// `None` for languages CodeGraph leaves NULL.
    pub get_signature: Option<StrHook>,
    /// Visibility of a definition (`public`/`private`/…). Applied to
    /// function/method/class/struct/enum/trait/property/field.
    pub get_visibility: Option<StrHook>,
    /// Whether a callable is async/suspend.
    pub is_async: Option<BoolHook>,
    /// Whether a member is static.
    pub is_static: Option<BoolHook>,
    /// A method's receiver type (extension methods) → used to override the
    /// qualified name to `Receiver::name` and add an owner `contains` edge.
    pub get_receiver_type: Option<StrHook>,
    /// Extract `(module_name, signature)` from an import node.
    pub extract_import: ImportHook,
    /// Extract a package/namespace name (JVM languages) from a package node.
    pub extract_package: Option<StrHook>,
    /// Locate a definition's body subtree when the grammar has no `body` field.
    pub resolve_body: Option<for<'a> fn(Node<'a>) -> Option<Node<'a>>>,
    /// Language-specific pre-dispatch handler. Returns `true` when it fully
    /// handled the node (the walker then skips its default dispatch + descent).
    pub visit_hook: Option<for<'a, 'b> fn(&'b mut Extractor<'a>, Node<'a>) -> bool>,
    /// Extract member nodes from a `field_types` declaration (per-language
    /// declarator shapes). `None` falls back to the generic identifier scan.
    pub extract_field: Option<for<'a, 'b> fn(&'b mut Extractor<'a>, Node<'a>)>,
    /// Derive a reference-site payload from a call/instantiation node.
    pub call_payload: for<'a> fn(Node<'a>, &str, bool) -> Option<SitePayload>,
    /// Ruby-style bare method calls: a statement-level `identifier` that is a
    /// call with neither parentheses nor receiver. Returns the callee name.
    pub bare_call: Option<StrHook>,
}

/// The recursive-descent extractor state for one file.
pub(crate) struct Extractor<'a> {
    pub src: &'a str,
    pub spec: &'a LangSpec,
    pub file_id: String,
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
    pub sites: Vec<RawSite>,
    /// Stack of enclosing emitted definitions: `(node id, kind, name)`.
    stack: Vec<(String, String, String)>,
    now: i64,
    db_lang: &'static str,
    stored_path: &'a str,
}

/// Named children of `node` as owned `Node<'a>` (tree-lifetime, not cursor-
/// bound), so they can outlive a local iteration scope. tree-sitter's
/// cursor-based `named_children` yields cursor-borrowed nodes; index access does
/// not.
fn named_children_of<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .collect()
}

/// Node kinds that count as "class-like" containers (methods nested in them are
/// methods, not free functions).
fn is_class_like(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "struct" | "interface" | "trait" | "enum" | "module"
    )
}

impl<'a> Extractor<'a> {
    /// The `::`-joined qualified name for `name` given the current scope stack
    /// (matching CodeGraph's `buildQualifiedName`: emitted non-file ancestors).
    pub fn qualified_name(&self, name: &str) -> String {
        let mut parts: Vec<&str> = self
            .stack
            .iter()
            .filter(|(_, kind, _)| kind != "file")
            .map(|(_, _, n)| n.as_str())
            .collect();
        parts.push(name);
        parts.join("::")
    }

    fn top_id(&self) -> String {
        self.stack
            .last()
            .map(|(id, _, _)| id.clone())
            .unwrap_or_else(|| self.file_id.clone())
    }

    pub fn inside_class_like(&self) -> bool {
        self.stack
            .last()
            .map(|(_, kind, _)| is_class_like(kind))
            .unwrap_or(false)
    }

    /// Create + push a node record with a `contains` edge from the current
    /// scope, returning its id. `qn_override` bypasses the stack-derived name.
    pub fn create_node(
        &mut self,
        kind: &str,
        name: &str,
        node: Node,
        qn_override: Option<String>,
    ) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        let qualified = qn_override.unwrap_or_else(|| self.qualified_name(name));
        let start = node.start_position();
        let end = node.end_position();
        let rec = NodeRecord::new(
            kind,
            name,
            &qualified,
            self.stored_path,
            self.db_lang,
            start.row as u32 + 1,
            end.row as u32 + 1,
            start.column as u32,
            end.column as u32,
            self.now,
        );
        let id = rec.id.clone();
        let parent = self.top_id();
        self.nodes.push(rec);
        self.edges.push(EdgeRecord::contains(parent, id.clone()));
        Some(id)
    }

    /// Push a scope entry (after [`Self::create_node`]).
    pub fn push_scope(&mut self, id: String, kind: &str, name: &str) {
        self.stack.push((id, kind.to_string(), name.to_string()));
    }

    pub fn pop_scope(&mut self) {
        self.stack.pop();
    }

    pub fn add_site(&mut self, site: RawSite) {
        self.sites.push(site);
    }

    /// The most recent node record (the one just created), for setting columns.
    fn last_node_mut(&mut self) -> &mut NodeRecord {
        self.nodes.last_mut().expect("a node was just created")
    }

    // ---- the visitor ----------------------------------------------------

    pub fn visit(&mut self, node: Node<'a>) {
        if let Some(hook) = self.spec.visit_hook {
            if hook(self, node) {
                return;
            }
        }
        let t = node.kind();
        let spec = self.spec;

        if spec.function_types.contains(&t) {
            if self.inside_class_like() && spec.method_types.contains(&t) {
                self.extract_method(node);
            } else {
                self.extract_function(node);
            }
        } else if spec.class_types.contains(&t) {
            let classification = spec
                .classify_class
                .map(|f| f(node, self.src))
                .unwrap_or(None);
            match classification.as_deref() {
                Some("struct") => self.extract_struct(node),
                Some("enum") => self.extract_enum(node),
                Some("interface") => self.extract_container(node, "interface", false),
                Some("trait") => self.extract_container(node, "trait", true),
                _ => self.extract_container(node, "class", true),
            }
        } else if spec.extra_class_node_types.contains(&t) {
            self.extract_container(node, "class", true);
        } else if spec.method_types.contains(&t) {
            self.extract_method(node);
        } else if spec.interface_types.contains(&t) {
            self.extract_container(node, spec.interface_kind, false);
        } else if spec.struct_types.contains(&t) {
            self.extract_struct(node);
        } else if spec.enum_types.contains(&t) {
            self.extract_enum(node);
        } else if spec.type_alias_types.contains(&t) {
            self.extract_type_alias(node);
        } else if spec.property_types.contains(&t) && self.inside_class_like() {
            self.extract_property(node);
        } else if spec.field_types.contains(&t) && self.inside_class_like() {
            if let Some(f) = spec.extract_field {
                f(self, node);
            } else {
                self.extract_field_generic(node);
            }
        } else if spec.variable_types.contains(&t) && !self.inside_class_like() {
            self.extract_variable(node);
        } else if spec.import_types.contains(&t) {
            self.extract_import(node);
            self.descend(node);
        } else if spec.call_types.contains(&t) {
            self.extract_call(node, false);
            self.descend(node);
        } else if spec.instantiation_types.contains(&t) {
            self.extract_call(node, true);
            self.descend(node);
        } else {
            self.descend(node);
        }
    }

    /// Definition-context descent (CodeGraph's `visitNode` traversal): used at
    /// the file root and inside class/interface/struct/enum bodies.
    fn descend(&mut self, node: Node<'a>) {
        let mut c = node.walk();
        for child in node.named_children(&mut c).collect::<Vec<_>>() {
            self.visit(child);
        }
    }

    /// Body-context descent (CodeGraph's `visitFunctionBody`): inside a
    /// function/method body, dispatch calls/instantiations/bare-calls and nested
    /// named definitions, recursing through everything else. Unlike the
    /// definition traversal this does **not** run the import dispatch — which is
    /// why a Ruby `foo(...)` inside a body is a `call` (an `importTypes`-shadowed
    /// no-op at statement level) here becomes a resolved call.
    fn descend_body(&mut self, node: Node<'a>) {
        let mut c = node.walk();
        for child in node.named_children(&mut c).collect::<Vec<_>>() {
            self.visit_body(child);
        }
    }

    fn visit_body(&mut self, node: Node<'a>) {
        let t = node.kind();
        let spec = self.spec;
        if spec.call_types.contains(&t) {
            self.extract_call(node, false);
            self.descend_body(node); // nested calls in argument positions
        } else if spec.instantiation_types.contains(&t) {
            self.extract_call(node, true);
            self.descend_body(node);
        } else if spec.function_types.contains(&t) {
            // A nested *named* function becomes its own node; anonymous ones fall
            // through so their inner calls stay attributed to the enclosing body.
            if self.extract_name(node).is_some() {
                self.extract_function(node);
            } else {
                self.descend_body(node);
            }
        } else if spec.class_types.contains(&t)
            || spec.struct_types.contains(&t)
            || spec.enum_types.contains(&t)
            || spec.interface_types.contains(&t)
        {
            // A local definition inside a body: reuse the definition dispatch.
            self.visit(node);
        } else {
            if let Some(bare) = spec.bare_call {
                if let Some(name) = bare(node, self.src) {
                    let from_id = self.top_id();
                    let start = node.start_position();
                    self.add_site(RawSite {
                        from_id,
                        line: start.row as u32 + 1,
                        col: start.column as u32,
                        payload: SitePayload::CallOrCtor { name },
                    });
                }
            }
            self.descend_body(node);
        }
    }

    fn body_of(&self, node: Node<'a>) -> Option<Node<'a>> {
        if let Some(f) = self.spec.resolve_body {
            if let Some(b) = f(node) {
                return Some(b);
            }
        }
        node.child_by_field_name(self.spec.body_field)
    }

    fn extract_function(&mut self, node: Node<'a>) {
        // A free function with an inferred receiver is really a method.
        if let Some(f) = self.spec.get_receiver_type {
            if f(node, self.src).is_some() {
                self.extract_method(node);
                return;
            }
        }
        let name = match self.extract_name(node) {
            Some(n) => n,
            None => return,
        };
        let (doc, sig, vis, is_async, is_static) = self.callable_props(node);
        if let Some(id) = self.create_node("function", &name, node, None) {
            self.apply_callable(doc, sig, vis, is_async, is_static);
            self.push_scope(id, "function", &name);
            if let Some(body) = self.body_of(node) {
                self.descend_body(body);
            }
            self.pop_scope();
        }
    }

    fn extract_method(&mut self, node: Node<'a>) {
        let receiver = self.spec.get_receiver_type.and_then(|f| f(node, self.src));
        if !self.inside_class_like() && receiver.is_none() {
            // Not a real method here — treat as a free function.
            self.extract_function(node);
            return;
        }
        let name = match self.extract_name(node) {
            Some(n) => n,
            None => return,
        };
        let (doc, sig, vis, is_async, is_static) = self.callable_props(node);
        let qn_override = receiver.as_ref().map(|r| format!("{r}::{name}"));
        if let Some(id) = self.create_node("method", &name, node, qn_override) {
            self.apply_callable(doc, sig, vis, is_async, is_static);
            // Extension method: also contained by its owning type, if present.
            if let Some(r) = &receiver {
                if !self.inside_class_like() {
                    if let Some(owner) = self.owner_type_id(r) {
                        self.edges.push(EdgeRecord::contains(owner, id.clone()));
                    }
                }
            }
            self.push_scope(id, "method", &name);
            if let Some(body) = self.body_of(node) {
                self.descend_body(body);
            }
            self.pop_scope();
        }
    }

    /// A class/interface/trait: create, then visit body children for members.
    fn extract_container(&mut self, node: Node<'a>, kind: &str, with_visibility: bool) {
        let name = match self.extract_name(node) {
            Some(n) => n,
            None => return,
        };
        let doc = self.docstring(node);
        let vis = if with_visibility {
            self.spec.get_visibility.and_then(|f| f(node, self.src))
        } else {
            None
        };
        if let Some(id) = self.create_node(kind, &name, node, None) {
            {
                let rec = self.last_node_mut();
                rec.docstring = doc;
                rec.visibility = vis;
            }
            self.push_scope(id, kind, &name);
            let body = self.body_of(node).unwrap_or(node);
            self.descend(body);
            self.pop_scope();
        }
    }

    fn extract_struct(&mut self, node: Node<'a>) {
        // Require a body — forward declarations / type references are not defs.
        let body = match self.body_of(node) {
            Some(b) => b,
            None => return,
        };
        let name = match self.extract_name(node) {
            Some(n) => n,
            None => return,
        };
        let doc = self.docstring(node);
        let vis = self.spec.get_visibility.and_then(|f| f(node, self.src));
        if let Some(id) = self.create_node("struct", &name, node, None) {
            {
                let rec = self.last_node_mut();
                rec.docstring = doc;
                rec.visibility = vis;
            }
            self.push_scope(id, "struct", &name);
            self.descend(body);
            self.pop_scope();
        }
    }

    fn extract_enum(&mut self, node: Node<'a>) {
        let body = match self.body_of(node) {
            Some(b) => b,
            None => return,
        };
        let name = match self.extract_name(node) {
            Some(n) => n,
            None => return,
        };
        let doc = self.docstring(node);
        let vis = self.spec.get_visibility.and_then(|f| f(node, self.src));
        if let Some(id) = self.create_node("enum", &name, node, None) {
            {
                let rec = self.last_node_mut();
                rec.docstring = doc;
                rec.visibility = vis;
            }
            self.push_scope(id, "enum", &name);
            let member_types = self.spec.enum_member_types;
            let children: Vec<Node> = {
                let mut c = body.walk();
                body.named_children(&mut c).collect()
            };
            for child in children {
                if member_types.contains(&child.kind()) {
                    self.extract_enum_members(child);
                } else {
                    self.visit(child);
                }
            }
            self.pop_scope();
        }
    }

    fn extract_enum_members(&mut self, node: Node<'a>) {
        if let Some(nn) = node.child_by_field_name("name") {
            let name = self.node_text(nn).to_string();
            self.create_node("enum_member", &name, node, None);
            return;
        }
        let kids: Vec<Node> = named_children_of(node)
            .into_iter()
            .filter(|ch| {
                matches!(
                    ch.kind(),
                    "simple_identifier" | "identifier" | "property_identifier" | "constant"
                )
            })
            .collect();
        if kids.is_empty() {
            // The node itself is the identifier.
            let name = self.node_text(node).to_string();
            self.create_node("enum_member", &name, node, None);
        } else {
            for ch in kids {
                let name = self.node_text(ch).to_string();
                self.create_node("enum_member", &name, ch, None);
            }
        }
    }

    fn extract_type_alias(&mut self, node: Node<'a>) {
        let name = match self.extract_name(node) {
            Some(n) => n,
            None => return,
        };
        let doc = self.docstring(node);
        if self.create_node("type_alias", &name, node, None).is_some() {
            self.last_node_mut().docstring = doc;
        }
    }

    /// C# property: `property_declaration` → `property` node, sig `Type Name`.
    fn extract_property(&mut self, node: Node<'a>) {
        let doc = self.docstring(node);
        let vis = self.spec.get_visibility.and_then(|f| f(node, self.src));
        let is_static = self
            .spec
            .is_static
            .map(|f| f(node, self.src))
            .unwrap_or(false);
        let name = match node.child_by_field_name("name") {
            Some(n) => self.node_text(n).to_string(),
            None => return,
        };
        // Type = first named child that isn't a modifier/name/accessor list.
        let type_text = named_children_of(node)
            .into_iter()
            .find(|ch| {
                !matches!(
                    ch.kind(),
                    "modifier"
                        | "modifiers"
                        | "identifier"
                        | "accessor_list"
                        | "accessors"
                        | "equals_value_clause"
                        | "attribute_list"
                )
            })
            .map(|ch| self.node_text(ch).to_string());
        let sig = match &type_text {
            Some(t) => format!("{t} {name}"),
            None => name.clone(),
        };
        if self.create_node("property", &name, node, None).is_some() {
            let rec = self.last_node_mut();
            rec.docstring = doc;
            rec.visibility = vis;
            rec.signature = Some(sig);
            rec.is_static = is_static;
        }
    }

    /// Generic field extraction (C#/Java style: variable_declaration →
    /// variable_declarator name, with a leading type child).
    fn extract_field_generic(&mut self, node: Node<'a>) {
        let doc = self.docstring(node);
        let vis = self.spec.get_visibility.and_then(|f| f(node, self.src));
        let is_static = self
            .spec
            .is_static
            .map(|f| f(node, self.src))
            .unwrap_or(false);

        // C#: field_declaration → variable_declaration → variable_declarator(s).
        let var_decl = named_children_of(node)
            .into_iter()
            .find(|ch| ch.kind() == "variable_declaration");
        let search = var_decl.unwrap_or(node);
        let type_text = named_children_of(search)
            .into_iter()
            .find(|ch| {
                !matches!(
                    ch.kind(),
                    "modifier"
                        | "modifiers"
                        | "variable_declarator"
                        | "variable_declaration"
                        | "attribute_list"
                )
            })
            .map(|ch| self.node_text(ch).to_string());
        let declarators: Vec<Node> = named_children_of(search)
            .into_iter()
            .filter(|ch| ch.kind() == "variable_declarator")
            .collect();
        for decl in declarators {
            let name_node = decl.child_by_field_name("name").or_else(|| {
                named_children_of(decl)
                    .into_iter()
                    .find(|ch| ch.kind() == "identifier")
            });
            let Some(name_node) = name_node else { continue };
            let name = self.node_text(name_node).to_string();
            let sig = match &type_text {
                Some(t) => format!("{t} {name}"),
                None => name.clone(),
            };
            if self.create_node("field", &name, decl, None).is_some() {
                let rec = self.last_node_mut();
                rec.docstring = doc.clone();
                rec.visibility = vis.clone();
                rec.signature = Some(sig);
                rec.is_static = is_static;
            }
        }
    }

    fn extract_variable(&mut self, node: Node<'a>) {
        // Assignment-style variables (Ruby): the *target* is `left`/`name`/first
        // child. Only a plain `identifier` target yields a `variable` — a
        // `constant` target (Ruby `PI = ...`) is dropped, exactly as CodeGraph's
        // generic fallback does. Language packs needing richer handling (Scala
        // val/var, PHP const) use their `visit_hook` instead.
        let target = node
            .child_by_field_name("left")
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| node.named_child(0));
        let Some(target) = target else { return };
        if target.kind() != "identifier" {
            return;
        }
        let name = self.node_text(target).to_string();
        if name.is_empty() {
            return;
        }
        let doc = self.docstring(node);
        if self.create_node("variable", &name, node, None).is_some() {
            self.last_node_mut().docstring = doc;
        }
    }

    fn extract_import(&mut self, node: Node<'a>) {
        let Some((module, signature)) = (self.spec.extract_import)(node, self.src) else {
            return;
        };
        if module.is_empty() {
            return;
        }
        let parent = self.top_id();
        if let Some(id) = self.create_node("import", &module, node, None) {
            self.last_node_mut().signature = Some(signature);
            let start = node.start_position();
            self.add_site(RawSite {
                from_id: parent,
                line: start.row as u32 + 1,
                col: start.column as u32,
                payload: SitePayload::Import {
                    target_id: id,
                    module,
                },
            });
        }
    }

    fn extract_call(&mut self, node: Node<'a>, is_ctor: bool) {
        let from_id = self.top_id();
        let start = node.start_position();
        if let Some(payload) = (self.spec.call_payload)(node, self.src, is_ctor) {
            self.add_site(RawSite {
                from_id,
                line: start.row as u32 + 1,
                col: start.column as u32,
                payload,
            });
        }
    }

    // ---- shared derivations --------------------------------------------

    fn callable_props(
        &self,
        node: Node<'a>,
    ) -> (Option<String>, Option<String>, Option<String>, bool, bool) {
        let doc = self.docstring(node);
        let sig = self.spec.get_signature.and_then(|f| f(node, self.src));
        let vis = self.spec.get_visibility.and_then(|f| f(node, self.src));
        let is_async = self
            .spec
            .is_async
            .map(|f| f(node, self.src))
            .unwrap_or(false);
        let is_static = self
            .spec
            .is_static
            .map(|f| f(node, self.src))
            .unwrap_or(false);
        (doc, sig, vis, is_async, is_static)
    }

    fn apply_callable(
        &mut self,
        doc: Option<String>,
        sig: Option<String>,
        vis: Option<String>,
        is_async: bool,
        is_static: bool,
    ) {
        let rec = self.last_node_mut();
        rec.docstring = doc;
        rec.signature = sig;
        rec.visibility = vis;
        rec.is_async = is_async;
        rec.is_static = is_static;
    }

    /// The id of a struct/class/enum/trait named `name` in this file (for the
    /// extension-method owner `contains` edge).
    fn owner_type_id(&self, name: &str) -> Option<String> {
        self.nodes
            .iter()
            .find(|n| {
                n.name == name && matches!(n.kind.as_str(), "struct" | "class" | "enum" | "trait")
            })
            .map(|n| n.id.clone())
    }

    /// Extract a definition's name: the `name_field`, else the first name-ish
    /// named child (mirrors CodeGraph's `extractName` fallback).
    pub fn extract_name(&self, node: Node<'a>) -> Option<String> {
        if let Some(f) = self.spec.resolve_name {
            if let Some(n) = f(node, self.src) {
                if !n.is_empty() {
                    return Some(n);
                }
            }
        }
        if let Some(n) = node.child_by_field_name(self.spec.name_field) {
            let t = self.node_text(n);
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            if matches!(
                child.kind(),
                "identifier" | "type_identifier" | "simple_identifier" | "constant" | "name"
            ) {
                let t = self.node_text(child);
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    pub fn node_text(&self, node: Node) -> &'a str {
        &self.src[node.start_byte()..node.end_byte()]
    }

    /// The docstring preceding `node`: the contiguous run of comment siblings
    /// immediately above it, cleaned exactly as CodeGraph's
    /// `getPrecedingDocstring` does (see [`clean_doc_comment`]).
    fn docstring(&self, node: Node<'a>) -> Option<String> {
        let mut comments: Vec<String> = Vec::new();
        let mut sib = node.prev_named_sibling();
        while let Some(s) = sib {
            if matches!(
                s.kind(),
                "comment" | "line_comment" | "block_comment" | "documentation_comment"
            ) {
                comments.push(self.node_text(s).to_string());
                sib = s.prev_named_sibling();
            } else {
                break;
            }
        }
        if comments.is_empty() {
            return None;
        }
        comments.reverse();
        let joined = comments
            .iter()
            .map(|c| clean_doc_comment(c))
            .collect::<Vec<_>>()
            .join("\n");
        let joined = joined.trim().to_string();
        Some(joined)
    }
}

/// Clean one comment's text exactly as CodeGraph's `getPrecedingDocstring`:
/// strip a leading `/**`|`/*` and trailing `*/`, then per line strip a leading
/// `//` + optional single space and a leading `*` + optional single space.
///
/// This deliberately reproduces 0.9.7's quirks so docstrings match the golden
/// byte-for-byte: a `///` line keeps a stray leading `/` (the `//`-strip removes
/// only two slashes), and Ruby `#` comments are left untouched.
fn clean_doc_comment(raw: &str) -> String {
    // Strip a leading /** or /* and a trailing */ (whole-string, like the JS
    // regex `/^\/\*\*?|\*\/$/g`).
    let mut s = raw.to_string();
    if let Some(rest) = s.strip_prefix("/**") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("/*") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_suffix("*/") {
        s = rest.to_string();
    }
    // Per-line: strip leading `//` + optional space, then leading `*` + space.
    let cleaned: Vec<String> = s
        .split('\n')
        .map(|line| {
            let mut l = line;
            if let Some(rest) = l.strip_prefix("//") {
                l = rest.strip_prefix(' ').unwrap_or(rest);
            } else {
                let trimmed = l.trim_start();
                if let Some(rest) = trimmed.strip_prefix('*') {
                    l = rest.strip_prefix(' ').unwrap_or(rest);
                }
            }
            l.to_string()
        })
        .collect();
    cleaned.join("\n").trim().to_string()
}

/// Whether `lang` is a tier-3 language handled by this module.
pub(crate) fn spec_for(lang: Language) -> Option<LangSpec> {
    match lang {
        Language::Ruby => Some(ruby::spec()),
        Language::Php => Some(php::spec()),
        Language::Kotlin => Some(kotlin::spec()),
        Language::Swift => Some(swift::spec()),
        Language::Scala => Some(scala::spec()),
        Language::CSharp => Some(csharp::spec()),
        _ => None,
    }
}

/// Extract one tier-3 file into a [`FileExtract`] (nodes + `contains` edges +
/// reference sites), mirroring [`crate::parse::extract`] for tier-1.
pub(crate) fn extract(
    src: &str,
    stored_path: &str,
    language: Language,
    mtime_millis: i64,
    now_millis: i64,
) -> FileExtract {
    let spec = spec_for(language).expect("tier-3 language");
    let db_lang = spec.language.db_name();

    let line_count = src.split('\n').count().max(1) as u32;
    let basename = Path::new(stored_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(stored_path);
    let file_node = NodeRecord::new(
        "file",
        basename,
        stored_path,
        stored_path,
        db_lang,
        1,
        line_count,
        0,
        0,
        now_millis,
    );
    let file_id = file_node.id.clone();

    let mut parser = Parser::new();
    parser
        .set_language(&(spec.grammar)())
        .expect("grammar/tree-sitter ABI compatible");

    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => {
            return FileExtract {
                file_record: file_record(
                    stored_path,
                    src,
                    db_lang,
                    mtime_millis,
                    now_millis,
                    1,
                    Some("[\"parse failed\"]".to_string()),
                ),
                nodes: vec![file_node],
                edges: Vec::new(),
                sites: Vec::new(),
            };
        }
    };

    let mut ex = Extractor {
        src,
        spec: &spec,
        file_id: file_id.clone(),
        nodes: vec![file_node],
        edges: Vec::new(),
        sites: Vec::new(),
        stack: vec![(file_id, "file".to_string(), basename.to_string())],
        now: now_millis,
        db_lang,
        stored_path,
    };

    let root = tree.root_node();
    // JVM package header (Kotlin): create a `namespace` node scoping the whole
    // file, so every top-level declaration's qualified name is package-prefixed
    // (matching CodeGraph's `extractFilePackage`).
    let mut pushed_package = false;
    if !spec.package_types.is_empty() {
        if let (Some(pkg_node), Some(extract_pkg)) = (
            named_children_of(root)
                .into_iter()
                .find(|c| spec.package_types.contains(&c.kind())),
            spec.extract_package,
        ) {
            if let Some(pkg_name) = extract_pkg(pkg_node, src) {
                if let Some(id) = ex.create_node("namespace", &pkg_name, pkg_node, None) {
                    ex.push_scope(id, "namespace", &pkg_name);
                    pushed_package = true;
                }
            }
        }
    }
    ex.descend(root);
    if pushed_package {
        ex.pop_scope();
    }

    let node_count = ex.nodes.len() as u64;
    let file_record = file_record(
        stored_path,
        src,
        db_lang,
        mtime_millis,
        now_millis,
        node_count,
        // Recoverable tree-sitter error recovery (e.g. Kotlin's benign MISSING
        // automatic-semicolon markers) is NOT reported as a file error: it would
        // spuriously populate `files.errors` where CodeGraph records none.
        None,
    );

    FileExtract {
        file_record,
        nodes: ex.nodes,
        edges: ex.edges,
        sites: ex.sites,
    }
}

fn file_record(
    stored_path: &str,
    src: &str,
    db_lang: &str,
    mtime_millis: i64,
    now_millis: i64,
    node_count: u64,
    errors: Option<String>,
) -> FileRecord {
    FileRecord {
        path: stored_path.to_string(),
        content_hash: content_hash(src.as_bytes()),
        language: db_lang.to_string(),
        size: src.len() as u64,
        modified_at: mtime_millis,
        indexed_at: now_millis,
        node_count,
        errors,
    }
}

/// The definition (`.scm`) and reference-site (`.refs.scm`) manifests for a
/// tier-3 language: the reviewed list of node types the engine treats as
/// definitions / call sites. Compiled against the grammar in the tests below so
/// a grammar upgrade that renames a node type fails CI loudly.
#[cfg(test)]
fn manifests(lang: Language) -> (&'static str, &'static str) {
    match lang {
        Language::Ruby => (
            include_str!("../queries/ruby.scm"),
            include_str!("../queries/ruby.refs.scm"),
        ),
        Language::Php => (
            include_str!("../queries/php.scm"),
            include_str!("../queries/php.refs.scm"),
        ),
        Language::Kotlin => (
            include_str!("../queries/kotlin.scm"),
            include_str!("../queries/kotlin.refs.scm"),
        ),
        Language::Swift => (
            include_str!("../queries/swift.scm"),
            include_str!("../queries/swift.refs.scm"),
        ),
        Language::Scala => (
            include_str!("../queries/scala.scm"),
            include_str!("../queries/scala.refs.scm"),
        ),
        Language::CSharp => (
            include_str!("../queries/csharp.scm"),
            include_str!("../queries/csharp.refs.scm"),
        ),
        _ => unreachable!("not a tier-3 language"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Query;

    const TIER3: &[Language] = &[
        Language::Ruby,
        Language::Php,
        Language::Kotlin,
        Language::Swift,
        Language::Scala,
        Language::CSharp,
    ];

    /// Parse the bare node-type tokens `(type)` out of a `.scm` manifest.
    fn scm_types(scm: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in scm.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('(') {
                let ty: String = rest.chars().take_while(|c| *c != ')').collect();
                if !ty.is_empty() {
                    out.push(ty);
                }
            }
        }
        out
    }

    /// Definition node types the engine handles via a language `visit_hook`
    /// rather than a `LangSpec` type set — allowed in a def manifest.
    const HOOK_DEF_TYPES: &[&str] = &[
        "module",                // Ruby module scope
        "const_declaration",     // PHP constants
        "val_definition",        // Scala val
        "var_definition",        // Scala var
        "enum_case_definitions", // Scala enum cases
    ];

    fn def_type_union(s: &LangSpec) -> std::collections::HashSet<&'static str> {
        let mut set = std::collections::HashSet::new();
        for grp in [
            s.function_types,
            s.method_types,
            s.class_types,
            s.interface_types,
            s.struct_types,
            s.enum_types,
            s.enum_member_types,
            s.type_alias_types,
            s.import_types,
            s.variable_types,
            s.field_types,
            s.property_types,
            s.extra_class_node_types,
            s.package_types,
        ] {
            set.extend(grp.iter().copied());
        }
        set.extend(HOOK_DEF_TYPES.iter().copied());
        set
    }

    /// Every tier-3 `.scm`/`.refs.scm` manifest compiles against its grammar —
    /// this is the drift alarm for a grammar-crate upgrade.
    #[test]
    fn manifests_compile_against_grammars() {
        for &lang in TIER3 {
            let spec = spec_for(lang).unwrap();
            let grammar = (spec.grammar)();
            let (def_scm, refs_scm) = manifests(lang);
            Query::new(&grammar, def_scm)
                .unwrap_or_else(|e| panic!("{lang:?} def manifest does not compile: {e:?}"));
            Query::new(&grammar, refs_scm)
                .unwrap_or_else(|e| panic!("{lang:?} refs manifest does not compile: {e:?}"));
        }
    }

    /// The definition/refs node types listed in each manifest are a subset of the
    /// spec's declared type sets — so the reviewed `.scm` cannot silently drift
    /// from the code the engine actually dispatches on. (A subset, not equality:
    /// Ruby imports ride on `call` nodes covered by the refs manifest, and hook-
    /// handled kinds like Scala `val`/`enum_case` are captured under def.)
    #[test]
    fn manifests_match_spec_types() {
        for &lang in TIER3 {
            let spec = spec_for(lang).unwrap();
            let (def_scm, refs_scm) = manifests(lang);
            let def_union = def_type_union(&spec);
            for ty in scm_types(def_scm) {
                assert!(
                    def_union.contains(ty.as_str()),
                    "{lang:?} def manifest lists `{ty}` not in the spec's definition types"
                );
            }
            let ref_union: std::collections::HashSet<&str> = spec
                .call_types
                .iter()
                .chain(spec.instantiation_types.iter())
                .copied()
                .collect();
            for ty in scm_types(refs_scm) {
                assert!(
                    ref_union.contains(ty.as_str()),
                    "{lang:?} refs manifest lists `{ty}` not in call/instantiation types"
                );
            }
        }
    }
}
