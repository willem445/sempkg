//! Tree-sitter parse + definition-extraction layer (issue #78, Phase 2a).
//!
//! For each supported source file this produces the schema-v4 nodes (a `file`
//! node plus every definition) and the structural `contains` edges that nest
//! them. Call/reference/import *edge resolution* is Phase 2b and deliberately
//! not built here — but the per-file symbol output (qualified names + ids) is
//! exactly what a pass-2 resolver will consume.
//!
//! ## Extraction model
//!
//! A small per-language `.scm` query (see `src/queries/`) captures definition
//! nodes; everything else — method-vs-function classification, qualified names
//! (`Outer::inner`, `::`-joined like CodeGraph), visibility/async/export flags,
//! docstrings, and nesting — is derived here in Rust by walking the captured
//! node's ancestors. This keeps the queries tiny and robust across grammar
//! revisions.
//!
//! ## `contains` edges
//!
//! Each node gets a `contains` edge from its nearest *emitted* enclosing
//! definition, or from the file node when there is none. Rust `impl` methods
//! additionally get an edge from the `impl`'s target type (struct/enum), so a
//! method is contained by both its file and its type — matching CodeGraph.
//!
//! The query capture conventions are adapted from CodeGraph's MIT-licensed
//! tree-sitter tag queries (https://github.com/colbymchenry/codegraph); see the
//! repository `NOTICE`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::model::{content_hash, EdgeRecord, FileRecord, Language, NodeRecord};

/// The nodes + edges extracted from one source file, plus its `files` row.
#[derive(Debug, Clone)]
pub struct FileExtract {
    pub file_record: FileRecord,
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
}

/// Extract definitions from `src`.
///
/// `stored_path` is the file's path as it will appear in the database (the
/// root-relative, namespaced form chosen by the indexer — see [`crate::index`]).
/// `mtime_millis` is the file's modification time; `now_millis` the index time.
pub fn extract(
    src: &str,
    stored_path: &str,
    language: Language,
    mtime_millis: i64,
    now_millis: i64,
) -> FileExtract {
    let db_lang = language.db_name();
    let (ts_language, query) = compiled(language);

    let mut parser = Parser::new();
    parser
        .set_language(ts_language)
        .expect("grammar/tree-sitter ABI compatible");

    let line_count = src.lines().count().max(1) as u32;
    let basename = Path::new(stored_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(stored_path);

    // The file node is always present, even for an unparyseable file.
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

    let mut nodes: Vec<NodeRecord> = vec![file_node];
    let mut edges: Vec<EdgeRecord> = Vec::new();

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
                    nodes.len() as u64,
                    Some("[\"parse failed\"]".to_string()),
                ),
                nodes,
                edges,
            };
        }
    };
    let root = tree.root_node();

    // 1. Collect captured definition nodes, deduped and in document order.
    let mut cursor = QueryCursor::new();
    let mut seen = HashSet::new();
    let mut raws: Vec<(Node, &str)> = Vec::new();
    let mut matches = cursor.matches(query, root, src.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let kind = cap_name.strip_prefix("def.").unwrap_or(cap_name);
            if seen.insert(cap.node.id()) {
                raws.push((cap.node, kind));
            }
        }
    }
    raws.sort_by_key(|(n, _)| (n.start_byte(), n.end_byte()));

    // 2. Build node records; remember tree-node id → record id for nesting, and
    //    struct/enum names → id for the Rust impl-type association.
    let mut ts_to_id: HashMap<usize, String> = HashMap::new();
    let mut type_nodes: HashMap<String, String> = HashMap::new();
    let mut emitted: Vec<(Node, usize)> = Vec::new();

    for (node, raw_kind) in &raws {
        let node = *node;
        let name = match def_name(node, raw_kind, src, language) {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let kind = reclassify(raw_kind, node, language);
        let qualified = qualified_name(node, &name, src, language);
        let start = node.start_position();
        let end = node.end_position();
        let mut rec = NodeRecord::new(
            &kind,
            &name,
            &qualified,
            stored_path,
            db_lang,
            start.row as u32 + 1,
            end.row as u32 + 1,
            start.column as u32,
            end.column as u32,
            now_millis,
        );
        apply_flags(&mut rec, node, src, language, &kind);
        rec.signature = first_line(node, src);
        if language == Language::Python {
            rec.docstring = python_docstring(node, src);
        }

        let idx = nodes.len();
        ts_to_id.insert(node.id(), rec.id.clone());
        if kind == "struct" || kind == "enum" {
            type_nodes.entry(rec.name.clone()).or_insert(rec.id.clone());
        }
        nodes.push(rec);
        emitted.push((node, idx));
    }

    // 3. Structural `contains` edges.
    for (node, idx) in &emitted {
        let child_id = nodes[*idx].id.clone();
        let parent_id =
            nearest_emitted_ancestor(*node, &ts_to_id).unwrap_or_else(|| file_id.clone());
        edges.push(EdgeRecord::contains(parent_id.clone(), child_id.clone()));

        // Rust: a method is also contained by its impl's target type.
        if language == Language::Rust && nodes[*idx].kind == "method" {
            if let Some(type_name) = enclosing_impl_type(*node, src) {
                if let Some(type_id) = type_nodes.get(&type_name) {
                    if *type_id != parent_id {
                        edges.push(EdgeRecord::contains(type_id.clone(), child_id.clone()));
                    }
                }
            }
        }
    }

    let errors = if root.has_error() {
        Some("[\"syntax errors present\"]".to_string())
    } else {
        None
    };
    let file_record = file_record(
        stored_path,
        src,
        db_lang,
        mtime_millis,
        now_millis,
        nodes.len() as u64,
        errors,
    );

    FileExtract {
        file_record,
        nodes,
        edges,
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

// ---- name / qualified-name derivation ------------------------------------

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

fn field_text(node: Node, field: &str, src: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| node_text(n, src).to_string())
}

/// The definition's own (unqualified) name, or `None` to drop it (e.g. a
/// `from __future__ import` that CodeGraph also elides).
fn def_name(node: Node, kind: &str, src: &str, lang: Language) -> Option<String> {
    match kind {
        "import" => import_name(node, src, lang),
        "variable" => field_text(node, "left", src).or_else(|| field_text(node, "name", src)),
        "enum_member" => {
            field_text(node, "name", src).or_else(|| Some(node_text(node, src).to_string()))
        }
        _ => field_text(node, "name", src),
    }
}

/// Reclassify a captured `function` as a `method` when it is nested in a
/// type/impl/class body.
fn reclassify(kind: &str, node: Node, lang: Language) -> String {
    if kind == "function" && has_method_container(node, lang) {
        return "method".to_string();
    }
    kind.to_string()
}

fn has_method_container(node: Node, lang: Language) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if is_method_container(n.kind(), lang) {
            return true;
        }
        cur = n.parent();
    }
    false
}

fn is_method_container(kind: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => matches!(kind, "impl_item" | "trait_item"),
        Language::Python => kind == "class_definition",
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            matches!(kind, "class_declaration" | "abstract_class_declaration")
        }
    }
}

/// Build the `::`-joined qualified name by walking enclosing type containers.
fn qualified_name(node: Node, name: &str, src: &str, lang: Language) -> String {
    let mut parts = Vec::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        if let Some(container) = qual_container_name(n, src, lang) {
            parts.push(container);
        }
        cur = n.parent();
    }
    if parts.is_empty() {
        name.to_string()
    } else {
        parts.reverse();
        format!("{}::{}", parts.join("::"), name)
    }
}

fn qual_container_name(node: Node, src: &str, lang: Language) -> Option<String> {
    match lang {
        Language::Rust => match node.kind() {
            "impl_item" => {
                let t = node.child_by_field_name("type")?;
                let txt = node_text(t, src);
                // Strip generics and any path prefix: `geometry::Point<T>` → `Point`.
                let base = txt.split('<').next().unwrap_or(txt).trim();
                Some(base.rsplit("::").next().unwrap_or(base).to_string())
            }
            "struct_item" | "enum_item" | "trait_item" | "mod_item" => {
                field_text(node, "name", src)
            }
            _ => None,
        },
        Language::Python => match node.kind() {
            "class_definition" => field_text(node, "name", src),
            _ => None,
        },
        Language::TypeScript | Language::Tsx | Language::JavaScript => match node.kind() {
            "class_declaration"
            | "abstract_class_declaration"
            | "enum_declaration"
            | "interface_declaration" => field_text(node, "name", src),
            _ => None,
        },
    }
}

/// The name of the type in the nearest enclosing Rust `impl` block, if any.
fn enclosing_impl_type(node: Node, src: &str) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "impl_item" {
            let t = n.child_by_field_name("type")?;
            let txt = node_text(t, src);
            let base = txt.split('<').next().unwrap_or(txt).trim();
            return Some(base.rsplit("::").next().unwrap_or(base).to_string());
        }
        cur = n.parent();
    }
    None
}

fn nearest_emitted_ancestor(node: Node, ts_to_id: &HashMap<usize, String>) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if let Some(id) = ts_to_id.get(&n.id()) {
            return Some(id.clone());
        }
        cur = n.parent();
    }
    None
}

// ---- import naming --------------------------------------------------------

/// Extract the module name an import refers to, matching CodeGraph's naming
/// (first path segment for Rust `use`, module for Python `from`, source string
/// for TS). Returns `None` for imports CodeGraph elides (`from __future__`).
fn import_name(node: Node, src: &str, lang: Language) -> Option<String> {
    match lang {
        Language::Rust => {
            // `use geometry::{...};` → `geometry` (first path segment).
            let text = node_text(node, src);
            let rest = text.strip_prefix("use").unwrap_or(text).trim_start();
            let seg: String = rest
                .chars()
                .take_while(|c| !matches!(c, ':' | '{' | ';' | ' ' | '\t' | '\n' | '('))
                .collect();
            let seg = seg.trim();
            if seg.is_empty() {
                None
            } else {
                Some(seg.to_string())
            }
        }
        Language::Python => {
            if node.kind() == "import_from_statement" {
                let module = field_text(node, "module_name", src)?;
                if module == "__future__" {
                    return None;
                }
                Some(module)
            } else {
                // `import asyncio` / `import a.b as c` → first module's dotted name.
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    match child.kind() {
                        "dotted_name" => return Some(node_text(child, src).to_string()),
                        "aliased_import" => {
                            let name = child.child_by_field_name("name")?;
                            return Some(node_text(name, src).to_string());
                        }
                        _ => {}
                    }
                }
                None
            }
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            // `import { ... } from "./geometry"` → `./geometry`.
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                if child.kind() == "string" {
                    let raw = node_text(child, src);
                    return Some(
                        raw.trim_matches(|c| c == '"' || c == '\'' || c == '`')
                            .to_string(),
                    );
                }
            }
            None
        }
    }
}

// ---- flags / signature / docstring ---------------------------------------

fn first_line(node: Node, src: &str) -> Option<String> {
    let text = node_text(node, src);
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

fn apply_flags(rec: &mut NodeRecord, node: Node, src: &str, lang: Language, kind: &str) {
    let header = node_text(node, src).lines().next().unwrap_or("");
    match lang {
        Language::Rust => {
            if matches!(kind, "function" | "method" | "struct" | "enum")
                && has_child_kind(node, "visibility_modifier")
            {
                rec.visibility = Some("public".to_string());
            }
            rec.is_async = header_has_word(header, "async");
        }
        Language::Python => {
            rec.is_async = header.trim_start().starts_with("async");
            if let Some(list) = python_decorators(node, src) {
                rec.decorators = Some(list);
            }
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            rec.is_exported = is_exported_ts(node);
            rec.is_async = header_has_word(header, "async");
            rec.is_static = header_has_word(header, "static");
            rec.is_abstract = header_has_word(header, "abstract");
        }
    }
    if let Some(tp) = type_parameters(node, src) {
        rec.type_parameters = Some(tp);
    }
}

fn header_has_word(header: &str, word: &str) -> bool {
    header
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| w == word)
}

fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut c = node.walk();
    let found = node.children(&mut c).any(|ch| ch.kind() == kind);
    found
}

fn is_exported_ts(node: Node) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "export_statement" {
            return true;
        }
        // Stop at the first non-wrapping ancestor to avoid false positives from
        // a distant export.
        if !matches!(n.kind(), "program" | "statement_block") {
            break;
        }
        cur = n.parent();
    }
    false
}

/// Collect `@decorator` names from a Python `decorated_definition` wrapper into
/// a JSON array string.
fn python_decorators(node: Node, src: &str) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() != "decorated_definition" {
        return None;
    }
    let mut names = Vec::new();
    let mut c = parent.walk();
    for child in parent.named_children(&mut c) {
        if child.kind() == "decorator" {
            names.push(node_text(child, src).trim().to_string());
        }
    }
    if names.is_empty() {
        None
    } else {
        serde_json::to_string(&names).ok()
    }
}

/// The first string literal in a Python function/class body, as a docstring.
fn python_docstring(node: Node, src: &str) -> Option<String> {
    if !matches!(node.kind(), "function_definition" | "class_definition") {
        return None;
    }
    let body = node.child_by_field_name("body")?;
    let mut c = body.walk();
    let first = body.named_children(&mut c).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let mut cc = first.walk();
    let string_node = first.named_children(&mut cc).next()?;
    if string_node.kind() != "string" {
        return None;
    }
    let raw = node_text(string_node, src);
    Some(strip_py_string(raw))
}

/// Strip Python string quotes/prefixes for a docstring's stored text.
fn strip_py_string(raw: &str) -> String {
    let trimmed = raw.trim_start_matches(['r', 'R', 'b', 'B', 'u', 'U', 'f', 'F']);
    let trimmed = trimmed.trim();
    for q in ["\"\"\"", "'''", "\"", "'"] {
        if let Some(inner) = trimmed.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.trim().to_string();
        }
    }
    trimmed.trim().to_string()
}

/// Extract generic parameter names (`<T, U>`) into a JSON array string.
fn type_parameters(node: Node, src: &str) -> Option<String> {
    let tp = node.child_by_field_name("type_parameters")?;
    let mut names = Vec::new();
    let mut c = tp.walk();
    for child in tp.named_children(&mut c) {
        match child.kind() {
            // Rust: type_identifier / lifetime / constrained_type_parameter, etc.
            "type_identifier" | "constrained_type_parameter" | "type_parameter" => {
                names.push(node_text(child, src).to_string());
            }
            _ => {}
        }
    }
    if names.is_empty() {
        None
    } else {
        serde_json::to_string(&names).ok()
    }
}

// ---- compiled grammar/query registry -------------------------------------

fn ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
        Language::Python => tree_sitter::Language::new(tree_sitter_python::LANGUAGE),
        Language::TypeScript => {
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
        Language::Tsx | Language::JavaScript => {
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TSX)
        }
    }
}

const RUST_QUERY: &str = include_str!("queries/rust.scm");
const PYTHON_QUERY: &str = include_str!("queries/python.scm");
const TYPESCRIPT_QUERY: &str = include_str!("queries/typescript.scm");

fn query_source(lang: Language) -> &'static str {
    match lang {
        Language::Rust => RUST_QUERY,
        Language::Python => PYTHON_QUERY,
        Language::TypeScript | Language::Tsx | Language::JavaScript => TYPESCRIPT_QUERY,
    }
}

/// Compile (and cache) the grammar + query for a language. Both
/// `tree_sitter::Language` and `Query` are `Sync`, so the cached pair is shared
/// across rayon worker threads; each thread still uses its own `Parser`.
fn compiled(lang: Language) -> &'static (tree_sitter::Language, Query) {
    macro_rules! cache {
        ($cell:ident) => {{
            static $cell: OnceLock<(tree_sitter::Language, Query)> = OnceLock::new();
            $cell.get_or_init(|| {
                let l = ts_language(lang);
                let q = Query::new(&l, query_source(lang)).expect("query compiles");
                (l, q)
            })
        }};
    }
    match lang {
        Language::Rust => cache!(RUST),
        Language::Python => cache!(PYTHON),
        Language::TypeScript => cache!(TS),
        Language::Tsx => cache!(TSX),
        Language::JavaScript => cache!(JS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: extract and return the non-file nodes keyed by qualified name.
    fn extract_nodes(src: &str, path: &str, lang: Language) -> Vec<NodeRecord> {
        extract(src, path, lang, 0, 0).nodes
    }

    fn find<'a>(nodes: &'a [NodeRecord], qn: &str) -> Option<&'a NodeRecord> {
        nodes
            .iter()
            .find(|n| n.qualified_name == qn && n.kind != "file")
    }

    #[test]
    fn rust_extracts_defs_with_qualified_names_and_visibility() {
        let src = r#"
pub type Scalar = f64;
pub struct Point { pub x: Scalar }
impl Point {
    pub fn new() -> Self { Point { x: 0.0 } }
}
pub enum Shape { Circle, Empty }
pub async fn go() {}
use geometry::{hypot, Point};
"#;
        let nodes = extract_nodes(src, "a.rs", Language::Rust);
        let kinds: std::collections::HashMap<&str, usize> =
            nodes
                .iter()
                .fold(std::collections::HashMap::new(), |mut m, n| {
                    *m.entry(n.kind.as_str()).or_default() += 1;
                    m
                });
        assert_eq!(kinds.get("type_alias"), Some(&1));
        assert_eq!(kinds.get("struct"), Some(&1));
        assert_eq!(kinds.get("enum"), Some(&1));
        assert_eq!(kinds.get("enum_member"), Some(&2));
        assert_eq!(kinds.get("import"), Some(&1));

        // Method inside impl is qualified and public.
        let new = find(&nodes, "Point::new").expect("Point::new");
        assert_eq!(new.kind, "method");
        assert_eq!(new.visibility.as_deref(), Some("public"));

        // Enum members are `Shape::Circle` etc.
        assert!(find(&nodes, "Shape::Circle").is_some());

        // async free fn flagged.
        let go = find(&nodes, "go").expect("go");
        assert!(go.is_async, "async fn should set is_async");

        // `use geometry::...` → import named by first segment.
        assert!(find(&nodes, "geometry").is_some());
    }

    #[test]
    fn rust_impl_method_contained_by_file_and_type() {
        let src = "pub struct P {}\nimpl P { pub fn m(&self) {} }\n";
        let ex = extract(src, "a.rs", Language::Rust, 0, 0);
        let method = ex
            .nodes
            .iter()
            .find(|n| n.qualified_name == "P::m")
            .unwrap();
        let struct_id = &ex
            .nodes
            .iter()
            .find(|n| n.qualified_name == "P" && n.kind == "struct")
            .unwrap()
            .id;
        let file_id = &ex.nodes.iter().find(|n| n.kind == "file").unwrap().id;
        let parents: Vec<&str> = ex
            .edges
            .iter()
            .filter(|e| e.kind == "contains" && e.target == method.id)
            .map(|e| e.source.as_str())
            .collect();
        assert!(
            parents.contains(&file_id.as_str()),
            "file should contain method"
        );
        assert!(
            parents.contains(&struct_id.as_str()),
            "struct should contain method"
        );
    }

    #[test]
    fn python_methods_docstrings_variables_and_future_skip() {
        let src = r#"
from __future__ import annotations
import asyncio
from shapes import Circle

Scalar = float

class Report:
    """Report docstring."""
    def measure(self, r):
        """measure doc"""
        return r

async def gather():
    await asyncio.sleep(0)
"#;
        let nodes = extract_nodes(src, "m.py", Language::Python);
        // __future__ import is elided; asyncio and shapes are kept.
        assert!(find(&nodes, "__future__").is_none());
        assert!(find(&nodes, "asyncio").is_some());
        assert!(find(&nodes, "shapes").is_some());
        // Module-level assignment is a variable.
        let scalar = find(&nodes, "Scalar").unwrap();
        assert_eq!(scalar.kind, "variable");
        // Method is qualified and carries its docstring.
        let measure = find(&nodes, "Report::measure").unwrap();
        assert_eq!(measure.kind, "method");
        assert_eq!(measure.docstring.as_deref(), Some("measure doc"));
        // Class docstring captured.
        assert_eq!(
            find(&nodes, "Report").unwrap().docstring.as_deref(),
            Some("Report docstring.")
        );
        // async def flagged.
        assert!(find(&nodes, "gather").unwrap().is_async);
    }

    #[test]
    fn typescript_exports_fields_enums_and_async() {
        let src = r#"
export type Scalar = number;
export enum Kind { Circle = "c", Empty = "e" }
export class Point {
  x: Scalar;
  constructor() { this.x = 0; }
  dist(): Scalar { return this.x; }
}
export async function go(): Promise<void> {}
import { hypot } from "./geometry";
"#;
        let nodes = extract_nodes(src, "a.ts", Language::TypeScript);
        assert_eq!(find(&nodes, "Scalar").unwrap().kind, "type_alias");
        assert!(find(&nodes, "Scalar").unwrap().is_exported);
        // Enum + members.
        assert_eq!(find(&nodes, "Kind").unwrap().kind, "enum");
        assert_eq!(find(&nodes, "Kind::Circle").unwrap().kind, "enum_member");
        // Class field recorded as a method (matching CodeGraph), qualified.
        assert_eq!(find(&nodes, "Point::x").unwrap().kind, "method");
        // constructor + method.
        assert!(find(&nodes, "Point::constructor").is_some());
        assert!(find(&nodes, "Point::dist").is_some());
        // async exported function.
        let go = find(&nodes, "go").unwrap();
        assert!(go.is_async && go.is_exported);
        // import by module specifier.
        assert!(find(&nodes, "./geometry").is_some());
    }

    #[test]
    fn file_node_always_present_and_spans_file() {
        let ex = extract("line1\nline2\nline3\n", "empty.py", Language::Python, 0, 0);
        let file = ex.nodes.iter().find(|n| n.kind == "file").unwrap();
        assert_eq!(file.name, "empty.py");
        assert_eq!(file.qualified_name, "empty.py");
        assert_eq!(file.start_line, 1);
        assert_eq!(file.end_line, 3);
        // node_count on the files row includes the file node itself.
        assert_eq!(ex.file_record.node_count, ex.nodes.len() as u64);
    }
}
