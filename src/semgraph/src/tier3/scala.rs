//! Scala language pack (issue #78 Phase 2c). Verified against CodeGraph 0.9.7
//! (see `src/queries/scala.scm`).
//!
//! Scala specifics:
//! - `class`/`object` → `class`, `trait` → `trait` ([`classify`]); a top-level
//!   `def` is a `function`, a `def`/`def`-declaration in a body is a `method`.
//! - `val`/`var` are handled by [`visit_hook`] (their name is in a `pattern`
//!   field): `constant`/`variable` at file scope, `field` inside a type, with a
//!   `val name: Type` signature.
//! - `enum` members come from `enum_case_definitions` (also via [`visit_hook`]).
//! - Unlike the other tier-3 packs, Scala function/method **signatures are
//!   populated** (`(params): ReturnType`) — CodeGraph defines a `getSignature`
//!   using the `parameters`/`return_type` fields. `package` clauses are ignored.

use tree_sitter::Node;

use super::{named_children_of, Extractor, LangSpec};
use crate::model::Language;
use crate::resolve::SitePayload;

pub(super) fn spec() -> LangSpec {
    LangSpec {
        language: Language::Scala,
        grammar: || tree_sitter::Language::new(tree_sitter_scala::LANGUAGE),
        function_types: &[],
        method_types: &["function_definition", "function_declaration"],
        class_types: &["class_definition", "object_definition", "trait_definition"],
        interface_types: &[],
        struct_types: &[],
        enum_types: &["enum_definition"],
        enum_member_types: &[],
        type_alias_types: &["type_definition"],
        import_types: &["import_declaration"],
        call_types: &["call_expression"],
        instantiation_types: &[],
        variable_types: &[],
        field_types: &[],
        property_types: &[],
        extra_class_node_types: &[],
        package_types: &[],
        interface_kind: "trait",
        name_field: "name",
        body_field: "body",
        resolve_name: None,
        classify_class: Some(classify),
        get_signature: Some(signature),
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
        bare_call: None,
        inheritance: Some(inheritance),
        type_refs: None,
    }
}

/// Scala inheritance: CodeGraph 0.9.7 records ONLY the primary parent — the first
/// type after `extends` — as an `extends` edge (whether it is a class or a
/// trait). The `with Trait` mixins that follow produce NO edge, and Scala never
/// emits `implements`. No Scala type references.
fn inheritance(node: Node, src: &str) -> Vec<super::InheritSite> {
    use super::{InheritEdge, InheritSite};
    let Some(ec) = node.child_by_field_name("extend") else {
        return Vec::new();
    };
    // The first `type` field is the primary parent; `with` mixins are ignored.
    let Some(first) = ec.child_by_field_name("type") else {
        return Vec::new();
    };
    let t = text(first, src);
    let name = t.rsplit('.').next().unwrap_or(t);
    let name = name.split(['[', '<']).next().unwrap_or(name);
    vec![InheritSite::at(name, first, InheritEdge::Extends)]
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

fn classify(node: Node, _src: &str) -> Option<String> {
    Some(if node.kind() == "trait_definition" {
        "trait".to_string()
    } else {
        "class".to_string()
    })
}

fn signature(node: Node, src: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters");
    let rt = node.child_by_field_name("return_type");
    if params.is_none() && rt.is_none() {
        return None;
    }
    let mut sig = params.map(|p| text(p, src).to_string()).unwrap_or_default();
    if let Some(rt) = rt {
        sig.push_str(": ");
        sig.push_str(text(rt, src));
    }
    if sig.is_empty() {
        None
    } else {
        Some(sig)
    }
}

fn visibility(node: Node, src: &str) -> Option<String> {
    for c in named_children_of(node) {
        if c.kind() == "modifiers" || c.kind() == "access_modifier" {
            let t = text(c, src);
            if t.contains("private") {
                return Some("private".to_string());
            }
            if t.contains("protected") {
                return Some("protected".to_string());
            }
        }
    }
    Some("public".to_string())
}

/// `import a.b.c` → the first `path` segment (matching CodeGraph, which returns
/// `childForFieldName('path')` — the first path child).
fn import(node: Node, src: &str) -> Option<(String, String)> {
    let sig = text(node, src).trim().to_string();
    if let Some(p) = node.child_by_field_name("path") {
        return Some((text(p, src).to_string(), sig));
    }
    let name = named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "identifier" || c.kind() == "stable_identifier")?;
    Some((text(name, src).to_string(), sig))
}

fn val_var_name(node: Node, src: &str) -> Option<String> {
    let pat = node.child_by_field_name("pattern")?;
    if pat.kind() == "identifier" {
        return Some(text(pat, src).to_string());
    }
    named_children_of(pat)
        .into_iter()
        .find(|c| c.kind() == "identifier")
        .map(|c| text(c, src).to_string())
}

fn visit_hook<'a>(ex: &mut Extractor<'a>, node: Node<'a>) -> bool {
    match node.kind() {
        "val_definition" | "var_definition" => {
            let Some(name) = val_var_name(node, ex.src) else {
                return true;
            };
            let in_class = ex.inside_class_like();
            let kind = if in_class {
                "field"
            } else if node.kind() == "val_definition" {
                "constant"
            } else {
                "variable"
            };
            let keyword = if node.kind() == "val_definition" {
                "val"
            } else {
                "var"
            };
            let sig = node
                .child_by_field_name("type")
                .map(|t| format!("{keyword} {name}: {}", text(t, ex.src)));
            let vis = visibility(node, ex.src);
            if ex.create_node(kind, &name, node, None).is_some() {
                let rec = ex.nodes.last_mut().unwrap();
                rec.signature = sig;
                rec.visibility = vis;
            }
            true
        }
        "enum_case_definitions" => {
            for child in named_children_of(node) {
                if matches!(child.kind(), "simple_enum_case" | "full_enum_case") {
                    if let Some(nn) = child.child_by_field_name("name") {
                        let name = ex.node_text(nn).to_string();
                        ex.create_node("enum_member", &name, child, None);
                    }
                }
            }
            true
        }
        "extension_definition" => {
            // No container node — visit the body's members directly, through the
            // depth-guarded walker.
            if let Some(body) = node.child_by_field_name("body") {
                ex.descend(body);
            }
            true
        }
        _ => false,
    }
}

fn call_payload(node: Node, src: &str, _is_ctor: bool) -> Option<SitePayload> {
    let func = node.child_by_field_name("function")?;
    if func.kind() == "identifier" {
        Some(SitePayload::CallOrCtor {
            name: text(func, src).to_string(),
        })
    } else {
        None
    }
}
