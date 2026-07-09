//! Kotlin language pack (issue #78 Phase 2c). Verified against CodeGraph 0.9.7
//! (see `src/queries/kotlin.scm`).
//!
//! Kotlin specifics:
//! - A `package` header wraps the file in a `namespace` node (handled generically
//!   by the engine via [`LangSpec::package_types`]/[`extract_package`]), so every
//!   top-level declaration's qualified name is `com.pkg::Name`.
//! - `class_declaration` is reused for classes, `interface`s and `enum`s
//!   ([`classify`]); `object` is a `class`.
//! - Extension functions `fun T.m()` name the method `m` and set receiver `T` (a
//!   leading `identifier` before the name); the method's qualified name is then
//!   overridden to `T::m` (never package-prefixed), matching CodeGraph.
//! - Signatures are NULL (CodeGraph's `getSignature` reads a node, not a field);
//!   `suspend` → async.
//!
//! Grammar: `tree-sitter-kotlin-ng`. Node types: `identifier` (names),
//! `qualified_identifier` (package/import paths), `import`, `type_alias`,
//! `class_declaration` (+`interface`/`enum_class_body` markers), `enum_entry`,
//! `class_body`/`enum_class_body`/`function_body`, `function_value_parameters`.

use tree_sitter::Node;

use super::{named_children_of, LangSpec};
use crate::model::Language;
use crate::resolve::SitePayload;

pub(super) fn spec() -> LangSpec {
    LangSpec {
        language: Language::Kotlin,
        grammar: || tree_sitter::Language::new(tree_sitter_kotlin_ng::LANGUAGE),
        function_types: &["function_declaration"],
        method_types: &["function_declaration"],
        class_types: &["class_declaration"],
        interface_types: &[],
        struct_types: &[],
        enum_types: &[],
        enum_member_types: &["enum_entry"],
        type_alias_types: &["type_alias"],
        import_types: &["import"],
        call_types: &["call_expression"],
        instantiation_types: &[],
        variable_types: &[],
        field_types: &[],
        property_types: &[],
        extra_class_node_types: &["object_declaration"],
        package_types: &["package_header"],
        interface_kind: "interface",
        name_field: "name",
        body_field: "class_body",
        resolve_name: Some(resolve_name),
        classify_class: Some(classify),
        get_signature: None,
        get_visibility: Some(visibility),
        is_async: Some(is_async),
        is_static: None,
        get_receiver_type: Some(receiver_type),
        extract_import: import,
        extract_package: Some(package),
        resolve_body: Some(resolve_body),
        visit_hook: None,
        extract_field: None,
        call_payload,
        bare_call: None,
        inheritance: Some(inheritance),
        type_refs: None,
    }
}

/// Kotlin inheritance from the `delegation_specifiers`: a `constructor_invocation`
/// delegate (`: Base(args)`, with a `()` call) is the superclass → `extends`; a
/// bare `user_type` delegate (`: Shape`) is an interface conformance →
/// `implements`. This is exactly how CodeGraph 0.9.7 distinguishes them
/// syntactically. No Kotlin type references.
fn inheritance(node: Node, src: &str) -> Vec<super::InheritSite> {
    use super::{named_children_of, InheritEdge, InheritSite};
    let mut out = Vec::new();
    let Some(ds) = named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "delegation_specifiers")
    else {
        return out;
    };
    for spec in named_children_of(ds) {
        if spec.kind() != "delegation_specifier" {
            continue;
        }
        let Some(child) = spec.named_child(0) else {
            continue;
        };
        let (edge, user_type) = match child.kind() {
            "constructor_invocation" => (InheritEdge::Extends, child.named_child(0)),
            "user_type" => (InheritEdge::Implements, Some(child)),
            _ => continue,
        };
        if let Some(ut) = user_type {
            if let Some(id) = user_type_ident(ut) {
                out.push(InheritSite::at(text(id, src), id, edge));
            }
        }
    }
    out
}

/// The leaf identifier of a Kotlin `user_type` (`Base` from `Base` / `pkg.Base`).
fn user_type_ident(ut: Node) -> Option<Node> {
    named_children_of(ut).into_iter().rev().find(|n| {
        matches!(
            n.kind(),
            "identifier" | "type_identifier" | "simple_identifier"
        )
    })
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

/// Identifier children appearing before the `function_value_parameters` node —
/// for a plain function this is `[name]`; for an extension `fun T.m()` it is
/// `[receiver, name]`.
fn idents_before_params<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    for ch in named_children_of(node) {
        if ch.kind() == "function_value_parameters" {
            break;
        }
        if ch.kind() == "identifier" {
            out.push(ch);
        }
    }
    out
}

/// The definition name. For a `function_declaration` it is the last identifier
/// before the parameter list (skips an extension receiver); otherwise the first
/// `identifier` child.
fn resolve_name(node: Node, src: &str) -> Option<String> {
    if node.kind() == "function_declaration" {
        return idents_before_params(node)
            .last()
            .map(|n| text(*n, src).to_string());
    }
    named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "identifier")
        .map(|n| text(n, src).to_string())
}

/// Extension-function receiver: `fun T.m()` parses as `user_type` (or bare
/// `identifier`) followed by a `.` token before the method name. The receiver is
/// the named child immediately preceding that dot.
fn receiver_type(node: Node, src: &str) -> Option<String> {
    let n = node.child_count();
    let mut prev: Option<Node> = None;
    for i in 0..n {
        let c = node.child(i)?;
        if c.kind() == "." {
            let recv = prev?;
            // Unwrap `user_type` → `type_identifier`/`identifier`.
            let inner = named_children_of(recv)
                .into_iter()
                .find(|x| matches!(x.kind(), "type_identifier" | "identifier"));
            return Some(
                inner
                    .map(|x| text(x, src))
                    .unwrap_or_else(|| text(recv, src))
                    .to_string(),
            );
        }
        if c.is_named() {
            prev = Some(c);
        }
    }
    None
}

/// `class_declaration` doubles as class / interface / enum: an `interface` token
/// child marks an interface; an `enum_class_body` child marks an enum (the `enum`
/// keyword itself sits inside `modifiers`).
fn classify(node: Node, _src: &str) -> Option<String> {
    let kids = named_children_of(node);
    if node.child_count() > 0
        && (0..node.child_count()).any(|i| {
            node.child(i)
                .map(|c| c.kind() == "interface")
                .unwrap_or(false)
        })
    {
        return Some("interface".to_string());
    }
    if kids.iter().any(|c| c.kind() == "enum_class_body") {
        return Some("enum".to_string());
    }
    Some("class".to_string())
}

/// Body is a `class_body` / `enum_class_body` / `function_body` child.
fn resolve_body(node: Node) -> Option<Node> {
    named_children_of(node)
        .into_iter()
        .find(|c| matches!(c.kind(), "function_body" | "class_body" | "enum_class_body"))
}

fn modifiers_text<'a>(node: Node, src: &'a str) -> Option<&'a str> {
    named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "modifiers")
        .map(|c| text(c, src))
}

fn visibility(node: Node, src: &str) -> Option<String> {
    if let Some(m) = modifiers_text(node, src) {
        if m.contains("private") {
            return Some("private".to_string());
        }
        if m.contains("protected") {
            return Some("protected".to_string());
        }
        if m.contains("internal") {
            return Some("internal".to_string());
        }
    }
    Some("public".to_string())
}

fn is_async(node: Node, src: &str) -> bool {
    modifiers_text(node, src)
        .map(|m| m.contains("suspend"))
        .unwrap_or(false)
}

fn package(node: Node, src: &str) -> Option<String> {
    named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "qualified_identifier" || c.kind() == "identifier")
        .map(|n| text(n, src).trim().to_string())
}

fn import(node: Node, src: &str) -> Option<(String, String)> {
    let sig = text(node, src).trim().to_string();
    let name = named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "qualified_identifier" || c.kind() == "identifier")?;
    Some((text(name, src).to_string(), sig))
}

fn call_payload(node: Node, src: &str, _is_ctor: bool) -> Option<SitePayload> {
    let callee = node.named_child(0)?;
    if callee.kind() == "identifier" {
        Some(SitePayload::CallOrCtor {
            name: text(callee, src).to_string(),
        })
    } else {
        // Member/navigation call (`recv.m()`): receiver type not inferred → drop.
        None
    }
}
