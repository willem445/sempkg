//! Swift language pack (issue #78 Phase 2c). Verified against CodeGraph 0.9.7
//! (see `src/queries/swift.scm`).
//!
//! Swift specifics:
//! - `class_declaration` is reused for `class`/`struct`/`enum` — [`classify`]
//!   distinguishes by a `struct`/`enum` keyword token or an `enum_class_body`.
//! - `protocol_declaration` → `interface`; protocol method *requirements* are
//!   `protocol_function_declaration` (not `function_declaration`), so they are
//!   not extracted — matching CodeGraph.
//! - Top-level `let`/`var` and struct stored properties are dropped (their name
//!   is a `pattern`, not an `identifier`), matching CodeGraph.
//! - Signatures are NULL (CodeGraph's `parameter` is a node, not a field);
//!   default visibility is `internal`.

use tree_sitter::Node;

use super::{named_children_of, LangSpec};
use crate::model::Language;
use crate::resolve::SitePayload;

pub(super) fn spec() -> LangSpec {
    LangSpec {
        language: Language::Swift,
        grammar: || tree_sitter::Language::new(tree_sitter_swift::LANGUAGE),
        function_types: &["function_declaration"],
        method_types: &["function_declaration"],
        class_types: &["class_declaration"],
        interface_types: &["protocol_declaration"],
        struct_types: &[],
        enum_types: &[],
        enum_member_types: &["enum_entry"],
        type_alias_types: &["typealias_declaration"],
        import_types: &["import_declaration"],
        call_types: &["call_expression"],
        instantiation_types: &[],
        variable_types: &[],
        field_types: &[],
        property_types: &[],
        extra_class_node_types: &[],
        package_types: &[],
        interface_kind: "interface",
        name_field: "name",
        body_field: "body",
        resolve_name: None,
        classify_class: Some(classify),
        get_signature: None,
        get_visibility: Some(visibility),
        is_async: Some(is_async),
        is_static: None,
        get_receiver_type: None,
        extract_import: import,
        extract_package: None,
        resolve_body: None,
        visit_hook: None,
        extract_field: None,
        call_payload,
        bare_call: None,
    }
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

/// `class_declaration` → class / struct / enum, by a `struct`/`enum` keyword
/// token child or an `enum_class_body`.
fn classify(node: Node, _src: &str) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i) {
            match c.kind() {
                "struct" => return Some("struct".to_string()),
                "enum" => return Some("enum".to_string()),
                _ => {}
            }
        }
    }
    if named_children_of(node)
        .iter()
        .any(|c| c.kind() == "enum_class_body")
    {
        return Some("enum".to_string());
    }
    Some("class".to_string())
}

fn visibility(node: Node, src: &str) -> Option<String> {
    for c in named_children_of(node) {
        if c.kind() == "modifiers" {
            let t = text(c, src);
            if t.contains("public") {
                return Some("public".to_string());
            }
            if t.contains("private") || t.contains("fileprivate") {
                return Some("private".to_string());
            }
            if t.contains("internal") {
                return Some("internal".to_string());
            }
        }
    }
    Some("internal".to_string())
}

fn is_async(node: Node, src: &str) -> bool {
    named_children_of(node)
        .iter()
        .any(|c| c.kind() == "modifiers" && text(*c, src).contains("async"))
}

fn import(node: Node, src: &str) -> Option<(String, String)> {
    let sig = text(node, src).trim().to_string();
    let id = named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "identifier")?;
    Some((text(id, src).to_string(), sig))
}

fn call_payload(node: Node, src: &str, _is_ctor: bool) -> Option<SitePayload> {
    let callee = node.named_child(0)?;
    if callee.kind() == "simple_identifier" {
        Some(SitePayload::CallOrCtor {
            name: text(callee, src).to_string(),
        })
    } else {
        None
    }
}
