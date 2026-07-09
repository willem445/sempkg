//! Ruby language pack (issue #78 Phase 2c). Node/edge conventions verified
//! against CodeGraph 0.9.7 output (see `src/queries/ruby.scm`).
//!
//! Ruby specifics the shared extractor relies on:
//! - `module` is a scope container (handled by [`visit_hook`]) so a class/method
//!   inside it gets a `Module::Class::method` qualified name.
//! - Top-level `def` is a `function`; a `def`/`self.def` inside a class/module is
//!   a `method`.
//! - `require`/`require_relative` are the only `call` nodes treated as imports;
//!   every other `call` (and a statement-level bare identifier) is a call site.
//!   CodeGraph checks imports before calls, so a top-level `foo(...)` is shadowed
//!   by the import dispatch and emits no call — only calls *inside bodies* count.
//! - Top-level `CONST = ...` assignments (a `constant` LHS) are dropped, matching
//!   CodeGraph; only a lowercase `identifier` LHS becomes a `variable`.

use tree_sitter::Node;

use super::LangSpec;
use crate::model::Language;
use crate::resolve::SitePayload;

pub(super) fn spec() -> LangSpec {
    LangSpec {
        language: Language::Ruby,
        grammar: || tree_sitter::Language::new(tree_sitter_ruby::LANGUAGE),
        function_types: &["method"],
        method_types: &["method", "singleton_method"],
        class_types: &["class"],
        interface_types: &[],
        struct_types: &[],
        enum_types: &[],
        enum_member_types: &[],
        type_alias_types: &[],
        import_types: &["call"],
        call_types: &["call", "method_call"],
        instantiation_types: &[],
        variable_types: &["assignment"],
        field_types: &[],
        property_types: &[],
        extra_class_node_types: &[],
        package_types: &[],
        interface_kind: "interface",
        name_field: "name",
        body_field: "body",
        resolve_name: None,
        classify_class: None,
        get_signature: None,
        get_visibility: Some(visibility),
        is_async: None,
        is_static: None,
        get_receiver_type: None,
        extract_import: import,
        extract_package: None,
        resolve_body: None,
        visit_hook: Some(visit_hook),
        extract_field: None,
        call_payload,
        bare_call: Some(bare_call),
    }
}

/// `module Foo ... end` — a scope container. Create a `module` node, then visit
/// its body as definitions so members get `Foo::…` qualified names.
fn visit_hook<'a>(ex: &mut super::Extractor<'a>, node: Node<'a>) -> bool {
    if node.kind() != "module" {
        return false;
    }
    let Some(name) = ex.extract_name(node) else {
        return true;
    };
    if let Some(id) = ex.create_node("module", &name, node, None) {
        ex.push_scope(id, "module", &name);
        if let Some(body) = node.child_by_field_name("body") {
            let mut c = body.walk();
            for child in body.named_children(&mut c).collect::<Vec<_>>() {
                ex.visit(child);
            }
        }
        ex.pop_scope();
    }
    true
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

/// Ruby visibility: scan preceding siblings for a bare `private`/`protected`/
/// `public` marker call; default `public`.
fn visibility(node: Node, src: &str) -> Option<String> {
    let mut sib = node.prev_named_sibling();
    while let Some(s) = sib {
        if s.kind() == "call" {
            if let Some(m) = s.child_by_field_name("method") {
                match text(m, src) {
                    "private" => return Some("private".to_string()),
                    "protected" => return Some("protected".to_string()),
                    "public" => return Some("public".to_string()),
                    _ => {}
                }
            }
        } else if s.kind() == "identifier" {
            match text(s, src) {
                "private" => return Some("private".to_string()),
                "protected" => return Some("protected".to_string()),
                "public" => return Some("public".to_string()),
                _ => {}
            }
        }
        sib = s.prev_named_sibling();
    }
    Some("public".to_string())
}

/// `require "x"` / `require_relative "x"` → import named by the string content.
fn import(node: Node, src: &str) -> Option<(String, String)> {
    let sig = text(node, src).trim().to_string();
    let mut c = node.walk();
    let method = node
        .named_children(&mut c)
        .find(|ch| ch.kind() == "identifier")?;
    let m = text(method, src);
    if m != "require" && m != "require_relative" {
        return None;
    }
    let mut c2 = node.walk();
    let args = node
        .named_children(&mut c2)
        .find(|ch| ch.kind() == "argument_list")?;
    let mut c3 = args.walk();
    let string = args
        .named_children(&mut c3)
        .find(|ch| ch.kind() == "string")?;
    let mut c4 = string.walk();
    let content = string
        .named_children(&mut c4)
        .find(|ch| ch.kind() == "string_content")?;
    Some((text(content, src).to_string(), sig))
}

/// Derive a call payload from a Ruby `call` node.
fn call_payload(node: Node, src: &str, _is_ctor: bool) -> Option<SitePayload> {
    let method = node.child_by_field_name("method")?;
    let name = text(method, src).to_string();
    match node.child_by_field_name("receiver") {
        None => Some(SitePayload::CallOrCtor { name }),
        Some(recv) => {
            let recv_text = text(recv, src);
            let last = recv_text.rsplit("::").next().unwrap_or(recv_text).trim();
            if name == "new" {
                // `Point.new` → instantiate the receiver type.
                Some(SitePayload::New {
                    name: last.to_string(),
                })
            } else {
                // `Foo.bar` / `Foo::bar` → qualified call `Foo::bar`.
                Some(SitePayload::QualifiedCall {
                    qualifier: last.to_string(),
                    name,
                })
            }
        }
    }
}

/// A statement-level bare method call: a plain `identifier` directly under a
/// block/body node, not a keyword/constant. Mirrors CodeGraph's `extractBareCall`.
fn bare_call(node: Node, src: &str) -> Option<String> {
    if node.kind() != "identifier" {
        return None;
    }
    let parent = node.parent()?;
    if !matches!(
        parent.kind(),
        "body_statement" | "then" | "else" | "do" | "begin" | "rescue" | "ensure" | "when"
    ) {
        return None;
    }
    let name = text(node, src);
    if matches!(
        name,
        "true" | "false" | "nil" | "self" | "super" | "__FILE__" | "__LINE__" | "__dir__"
    ) {
        return None;
    }
    // Constants (uppercase first byte) are class/module refs, not calls.
    let first = name.as_bytes().first().copied().unwrap_or(0);
    if first.is_ascii_uppercase() {
        return None;
    }
    Some(name.to_string())
}
