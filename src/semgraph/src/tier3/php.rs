//! PHP language pack (issue #78 Phase 2c). Verified against CodeGraph 0.9.7
//! (see `src/queries/php.scm`).
//!
//! PHP specifics: `trait`/`interface`/`enum` are first-class; class properties
//! are `field` nodes (`Type $name` signature); `const` (top-level and in-class)
//! is a `constant` node handled by [`visit_hook`]; `use App\X` is an `import`.
//! PHP function/method signatures are NULL (CodeGraph defines no `getSignature`).
//! `namespace` declarations do not scope qualified names (CodeGraph descends
//! into them without a scope node).

use tree_sitter::Node;

use super::{named_children_of, Extractor, LangSpec};
use crate::model::Language;
use crate::resolve::SitePayload;

pub(super) fn spec() -> LangSpec {
    LangSpec {
        language: Language::Php,
        grammar: || tree_sitter::Language::new(tree_sitter_php::LANGUAGE_PHP),
        function_types: &["function_definition"],
        method_types: &["method_declaration"],
        class_types: &["class_declaration", "trait_declaration"],
        interface_types: &["interface_declaration"],
        struct_types: &[],
        enum_types: &["enum_declaration"],
        enum_member_types: &["enum_case"],
        type_alias_types: &[],
        import_types: &["namespace_use_declaration"],
        call_types: &[
            "function_call_expression",
            "member_call_expression",
            "scoped_call_expression",
        ],
        instantiation_types: &["object_creation_expression"],
        variable_types: &[],
        field_types: &["property_declaration"],
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
        is_async: None,
        is_static: Some(is_static),
        get_receiver_type: None,
        extract_import: import,
        extract_package: None,
        resolve_body: None,
        visit_hook: Some(visit_hook),
        extract_field: Some(extract_field),
        call_payload,
        bare_call: None,
    }
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

fn classify(node: Node, _src: &str) -> Option<String> {
    Some(if node.kind() == "trait_declaration" {
        "trait".to_string()
    } else {
        "class".to_string()
    })
}

fn child_modifier(node: Node, kind: &str) -> bool {
    (0..node.child_count()).any(|i| node.child(i).map(|c| c.kind() == kind).unwrap_or(false))
}

fn visibility(node: Node, src: &str) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i) {
            if c.kind() == "visibility_modifier" {
                return Some(text(c, src).to_string());
            }
        }
    }
    Some("public".to_string())
}

fn is_static(node: Node, _src: &str) -> bool {
    child_modifier(node, "static_modifier")
}

/// `use App\Shapes\Circle;` → import named by the qualified/plain name.
fn import(node: Node, src: &str) -> Option<(String, String)> {
    let sig = text(node, src).trim().to_string();
    let clause = named_children_of(node)
        .into_iter()
        .find(|c| c.kind() == "namespace_use_clause")?;
    let name = named_children_of(clause)
        .into_iter()
        .find(|c| c.kind() == "qualified_name" || c.kind() == "name")?;
    Some((text(name, src).to_string(), sig))
}

/// Handle `const` (→ `constant` nodes; a `const_declaration` at any scope). Trait
/// `use TraitName;` and other statements fall through to the default walk.
fn visit_hook<'a>(ex: &mut Extractor<'a>, node: Node<'a>) -> bool {
    if node.kind() != "const_declaration" {
        return false;
    }
    for elem in named_children_of(node) {
        if elem.kind() != "const_element" {
            continue;
        }
        if let Some(nn) = named_children_of(elem)
            .into_iter()
            .find(|c| c.kind() == "name")
        {
            let name = ex.node_text(nn).to_string();
            ex.create_node("constant", &name, elem, None);
        }
    }
    true
}

/// PHP `property_declaration` → `field` node(s), signature `Type $name`.
fn extract_field<'a>(ex: &mut Extractor<'a>, node: Node<'a>) {
    let vis = visibility(node, ex.src);
    let stat = is_static(node, ex.src);
    // The declared type is the first named child that isn't a modifier or a
    // property_element.
    let type_text = named_children_of(node)
        .into_iter()
        .find(|c| {
            !matches!(
                c.kind(),
                "visibility_modifier"
                    | "static_modifier"
                    | "readonly_modifier"
                    | "var_modifier"
                    | "property_element"
                    | "attribute_list"
            )
        })
        .map(|c| ex.node_text(c).to_string());
    for elem in named_children_of(node) {
        if elem.kind() != "property_element" {
            continue;
        }
        let Some(var_name) = named_children_of(elem)
            .into_iter()
            .find(|c| c.kind() == "variable_name")
        else {
            continue;
        };
        let Some(nn) = named_children_of(var_name)
            .into_iter()
            .find(|c| c.kind() == "name")
        else {
            continue;
        };
        let name = ex.node_text(nn).to_string();
        let sig = match &type_text {
            Some(t) => format!("{t} ${name}"),
            None => format!("${name}"),
        };
        if ex.create_node("field", &name, elem, None).is_some() {
            let rec = ex.nodes.last_mut().unwrap();
            rec.visibility = vis.clone();
            rec.signature = Some(sig);
            rec.is_static = stat;
        }
    }
}

fn call_payload(node: Node, src: &str, is_ctor: bool) -> Option<SitePayload> {
    if is_ctor {
        // `new Circle(...)` → instantiate the named class.
        let name = named_children_of(node)
            .into_iter()
            .find(|c| c.kind() == "name" || c.kind() == "qualified_name")?;
        let t = text(name, src);
        let last = t.rsplit('\\').next().unwrap_or(t);
        return Some(SitePayload::New {
            name: last.to_string(),
        });
    }
    match node.kind() {
        "function_call_expression" => {
            let f = node.child_by_field_name("function")?;
            Some(SitePayload::CallOrCtor {
                name: text(f, src).to_string(),
            })
        }
        "scoped_call_expression" => {
            let scope = node.child_by_field_name("scope")?;
            let name = node.child_by_field_name("name")?;
            let q = text(scope, src);
            let qualifier = q.rsplit('\\').next().unwrap_or(q).trim_start_matches('$');
            Some(SitePayload::QualifiedCall {
                qualifier: qualifier.to_string(),
                name: text(name, src).to_string(),
            })
        }
        // `$obj->method()` — receiver type not inferred → dropped.
        _ => None,
    }
}
