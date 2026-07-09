//! C# language pack (issue #78 Phase 2c). Verified against CodeGraph 0.9.7
//! (see `src/queries/csharp.scm`).
//!
//! C# specifics:
//! - `namespace_declaration` does **not** scope qualified names (CodeGraph
//!   descends into its body without a scope node), so `App.Geo.Circle` is just
//!   `Circle`.
//! - Distinct node types per kind: `class`/`struct`/`interface`/`enum`; a
//!   `constructor_declaration` is a `method` named after the class; a
//!   `property_declaration` is a `property`, a `field_declaration` a `field`
//!   (both with a `Type Name` signature).
//! - `MathUtil.CircleArea(...)` resolves as a qualified call `MathUtil::CircleArea`;
//!   `new Circle(...)` is an instantiation. Local variables are dropped. Default
//!   visibility is `private`; method signatures are NULL.

use tree_sitter::Node;

use super::{named_children_of, LangSpec};
use crate::model::Language;
use crate::resolve::SitePayload;

pub(super) fn spec() -> LangSpec {
    LangSpec {
        language: Language::CSharp,
        grammar: || tree_sitter::Language::new(tree_sitter_c_sharp::LANGUAGE),
        function_types: &[],
        method_types: &["method_declaration", "constructor_declaration"],
        class_types: &["class_declaration"],
        interface_types: &["interface_declaration"],
        struct_types: &["struct_declaration"],
        enum_types: &["enum_declaration"],
        enum_member_types: &["enum_member_declaration"],
        type_alias_types: &[],
        import_types: &["using_directive"],
        call_types: &["invocation_expression"],
        instantiation_types: &["object_creation_expression"],
        variable_types: &[],
        field_types: &["field_declaration"],
        property_types: &["property_declaration"],
        extra_class_node_types: &[],
        package_types: &[],
        interface_kind: "interface",
        name_field: "name",
        body_field: "body",
        resolve_name: None,
        classify_class: None,
        get_signature: None,
        get_visibility: Some(visibility),
        is_async: Some(is_async),
        is_static: Some(is_static),
        get_receiver_type: None,
        extract_import: import,
        extract_package: None,
        resolve_body: None,
        visit_hook: None,
        extract_field: None,
        call_payload,
        bare_call: None,
        inheritance: Some(inheritance),
        type_refs: Some(type_refs),
    }
}

/// C# inheritance from the `base_list` (`class C : Base, IShape`). C# has no
/// syntactic base-vs-interface marker, so each entry is [`InheritEdge::Auto`]:
/// the resolver classifies an `interface` target as `implements`, a class base
/// as `extends`.
fn inheritance(node: Node, src: &str) -> Vec<super::InheritSite> {
    use super::{named_children_of, InheritEdge, InheritSite};
    let Some(bl) = named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "base_list")
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for n in named_children_of(bl) {
        if matches!(n.kind(), "identifier" | "qualified_name" | "generic_name") {
            out.push(InheritSite::at(base_name(n, src), n, InheritEdge::Auto));
        }
    }
    out
}

/// C# type references: CodeGraph 0.9.7 records a method's return type AND each
/// parameter type (user types only; a `predefined_type` like `double`/`void` is
/// not a graph node and produces no edge). The resolver drops those that do not
/// resolve to a type node.
fn type_refs(node: Node, src: &str) -> Vec<super::TypeRefSite> {
    use super::{named_children_of, TypeRefSite};
    let mut out = Vec::new();
    let mut push = |n: Node| {
        if matches!(n.kind(), "identifier" | "qualified_name" | "generic_name") {
            out.push(TypeRefSite::at(base_name(n, src), n));
        }
    };
    if let Some(r) = node.child_by_field_name("returns") {
        push(r);
    }
    if let Some(pl) = node.child_by_field_name("parameters") {
        for p in named_children_of(pl) {
            if p.kind() == "parameter" {
                if let Some(t) = p.child_by_field_name("type") {
                    push(t);
                }
            }
        }
    }
    out
}

/// The last dotted segment of a C# type name, with any generic `<…>` stripped.
fn base_name<'a>(node: Node, src: &'a str) -> &'a str {
    let t = text(node, src);
    let t = t.rsplit('.').next().unwrap_or(t);
    t.split('<').next().unwrap_or(t)
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

fn modifier_is(node: Node, src: &str, want: &str) -> bool {
    (0..node.child_count()).any(|i| {
        node.child(i)
            .map(|c| c.kind() == "modifier" && text(c, src) == want)
            .unwrap_or(false)
    })
}

fn visibility(node: Node, src: &str) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i) {
            if c.kind() == "modifier" {
                match text(c, src) {
                    "public" => return Some("public".to_string()),
                    "private" => return Some("private".to_string()),
                    "protected" => return Some("protected".to_string()),
                    "internal" => return Some("internal".to_string()),
                    _ => {}
                }
            }
        }
    }
    Some("private".to_string())
}

fn is_static(node: Node, src: &str) -> bool {
    modifier_is(node, src, "static")
}

fn is_async(node: Node, src: &str) -> bool {
    modifier_is(node, src, "async")
}

fn import(node: Node, src: &str) -> Option<(String, String)> {
    let sig = text(node, src).trim().to_string();
    let name = named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "qualified_name" || c.kind() == "identifier")?;
    Some((text(name, src).to_string(), sig))
}

fn call_payload(node: Node, src: &str, is_ctor: bool) -> Option<SitePayload> {
    if is_ctor {
        // `new Circle(...)` → the `type:` child names the class.
        let ty = node
            .child_by_field_name("type")
            .or_else(|| node.named_child(0))?;
        let t = text(ty, src);
        let last = t.rsplit('.').next().unwrap_or(t);
        return Some(SitePayload::New {
            name: last.to_string(),
        });
    }
    let func = node.child_by_field_name("function")?;
    match func.kind() {
        "identifier" => Some(SitePayload::CallOrCtor {
            name: text(func, src).to_string(),
        }),
        "member_access_expression" => {
            let expr = func.child_by_field_name("expression")?;
            let name = func.child_by_field_name("name")?;
            let q = text(expr, src);
            let qualifier = q.rsplit('.').next().unwrap_or(q);
            Some(SitePayload::QualifiedCall {
                qualifier: qualifier.to_string(),
                name: text(name, src).to_string(),
            })
        }
        _ => None,
    }
}
