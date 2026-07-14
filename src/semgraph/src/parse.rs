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
//! tree-sitter tag queries (<https://github.com/colbymchenry/codegraph>); see the
//! repository `NOTICE`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::model::{content_hash, EdgeRecord, FileRecord, Language, NodeRecord};
use crate::resolve::{bare_type_name, CallRef, RawSite, RecvExpr, SitePayload};

/// The nodes + edges extracted from one source file, plus its `files` row.
///
/// `sites` are the unresolved call/reference/import/instantiation occurrences
/// (Phase 2b); they carry call-site coordinates and whatever local context the
/// parser could infer, and are resolved to `calls`/`references`/`imports`/
/// `instantiates` edges by the resolver (`crate::resolve`) once every file is
/// parsed. `edges` here holds only the intra-file structural `contains` edges.
#[derive(Debug, Clone)]
pub struct FileExtract {
    pub file_record: FileRecord,
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
    pub(crate) sites: Vec<RawSite>,
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
    // Tier-3 languages (Ruby/PHP/Kotlin/Swift/Scala/C#) use the shared
    // config-driven recursive-descent extractor; only the tier-1 languages flow
    // through the query-plus-`match` path below.
    if language.is_tier3() {
        return crate::tier3::extract(src, stored_path, language, mtime_millis, now_millis);
    }

    let db_lang = language.db_name();
    let (ts_language, query) = compiled(language);

    let mut parser = Parser::new();
    parser
        .set_language(ts_language)
        .expect("grammar/tree-sitter ABI compatible");

    // File span end. CodeGraph counts lines as `content.split('\n').length`,
    // which includes the phantom empty segment after a trailing newline — so a
    // file whose bytes end in `\n` reports one more line than `str::lines()`
    // yields. Match that to keep file-node `end_line` byte-parity.
    let line_count = src.split('\n').count().max(1) as u32;
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
                sites: Vec::new(),
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
        // CodeGraph 0.9.7 records only file/module-level imports; a `use`/`import`
        // nested inside a function or closure body is not emitted. Match that.
        if kind == "import" && import_in_function_body(node, language) {
            continue;
        }
        // Tier-1 imports are named by their module root and NOT qualified by an
        // enclosing scope (a `use std::…;` inside `mod tests` is `std`, not
        // `tests::std`), matching CodeGraph 0.9.7. The tier-2/3 packs have their
        // own import conventions (e.g. CodeGraph package-qualifies Java imports as
        // `fixture::java.util.List`), so leave those to `qualified_name`.
        let qualified = if kind == "import" && language.is_tier1() {
            name.clone()
        } else {
            qualified_name(node, &name, src, language)
        };
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
        rec.signature = signature_for(node, &kind, src, language);
        rec.docstring = docstring_for(node, &kind, src, language);

        let idx = nodes.len();
        ts_to_id.insert(node.id(), rec.id.clone());
        if kind == "struct" || kind == "enum" {
            type_nodes.entry(rec.name.clone()).or_insert(rec.id.clone());
        }
        nodes.push(rec);
        emitted.push((node, idx));
    }

    // 3. Structural `contains` edges.
    //    Java attaches its top-level declarations to the file's `namespace`
    //    (package) node rather than directly to the file, matching CodeGraph.
    let namespace_id = (language == Language::Java)
        .then(|| {
            emitted.iter().find_map(|(_, idx)| {
                (nodes[*idx].kind == "namespace").then(|| nodes[*idx].id.clone())
            })
        })
        .flatten();
    for (node, idx) in &emitted {
        let child_id = nodes[*idx].id.clone();
        let mut parent_id =
            nearest_emitted_ancestor(*node, &ts_to_id).unwrap_or_else(|| file_id.clone());
        if let Some(ns) = &namespace_id {
            if parent_id == file_id && nodes[*idx].kind != "namespace" {
                parent_id = ns.clone();
            }
        }
        edges.push(EdgeRecord::contains(parent_id.clone(), child_id.clone()));

        // A method is also contained by its owning type when the type is defined
        // in the same file: Rust `impl` blocks and Go value/pointer receivers.
        if nodes[*idx].kind == "method" {
            let type_name = match language {
                Language::Rust => enclosing_impl_type(*node, src),
                Language::Go => go_receiver_type(*node, src),
                _ => None,
            };
            if let Some(type_name) = type_name {
                if let Some(type_id) = type_nodes.get(&type_name) {
                    if *type_id != parent_id {
                        edges.push(EdgeRecord::contains(type_id.clone(), child_id.clone()));
                    }
                }
            }
        }
    }

    // 4. Reference sites (Phase 2b): call/method/ctor sites via the refs query,
    //    type-reference sites walked over each definition's signature, and
    //    import sites reusing the emitted `import` nodes. These are resolved to
    //    edges after all files are parsed (see `crate::resolve`).
    let sites = collect_sites(
        root,
        src,
        language,
        &file_id,
        &ts_to_id,
        &emitted,
        &nodes,
        &type_nodes,
    );

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
        sites,
    }
}

// ---- reference-site collection (Phase 2b) --------------------------------

/// Collect every reference site in one file: call/method/constructor sites (via
/// the language's refs query), type-reference sites (walked over each
/// definition's signature), and import sites (from the emitted `import` nodes).
#[allow(clippy::too_many_arguments)]
fn collect_sites(
    root: Node,
    src: &str,
    lang: Language,
    file_id: &str,
    ts_to_id: &HashMap<usize, String>,
    emitted: &[(Node, usize)],
    nodes: &[NodeRecord],
    type_nodes: &HashMap<String, String>,
) -> Vec<RawSite> {
    let mut sites = Vec::new();

    // Map each emitted function/method definition's record id → its tree node,
    // so a call site can look up the locals of its enclosing callable.
    let mut def_node_by_id: HashMap<String, Node> = HashMap::new();
    for (node, idx) in emitted {
        if matches!(nodes[*idx].kind.as_str(), "function" | "method") {
            def_node_by_id.insert(nodes[*idx].id.clone(), *node);
        }
    }
    // record id → enclosing type name (for `self`/`this` receivers).
    let encl_type_by_id: HashMap<String, String> = emitted
        .iter()
        .filter_map(|(_, idx)| {
            let n = &nodes[*idx];
            if n.kind == "method" {
                n.qualified_name
                    .rsplit_once("::")
                    .map(|(ty, _)| (n.id.clone(), ty.to_string()))
            } else {
                None
            }
        })
        .collect();
    // Cache of locals maps, computed lazily per enclosing callable.
    let mut locals_cache: HashMap<String, HashMap<String, RecvExpr>> = HashMap::new();
    // type name → (field name → field type name), so a `self.field.method()`
    // receiver can be typed from the enclosing type's declared fields.
    let field_types = collect_field_types(root, src, lang);

    // 4a. Call / method / constructor sites via the refs query.
    let (_ts_lang, refs_query) = compiled_refs(lang);
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(refs_query, root, src.as_bytes());
    let mut raw_calls: Vec<(Node, &str)> = Vec::new();
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let cap_name = refs_query.capture_names()[cap.index as usize];
            raw_calls.push((cap.node, cap_name));
        }
    }
    // Deterministic order: document order by start byte.
    raw_calls.sort_by_key(|(n, _)| (n.start_byte(), n.end_byte()));

    for (call, cap) in &raw_calls {
        let call = *call;
        let from_id =
            nearest_emitted_ancestor(call, ts_to_id).unwrap_or_else(|| file_id.to_string());
        let start = call.start_position();
        let line = start.row as u32 + 1;
        let col = start.column as u32;

        let payload = if *cap == "new" {
            new_payload(call, src, lang)
        } else {
            call_payload(
                call,
                src,
                lang,
                &from_id,
                &def_node_by_id,
                &encl_type_by_id,
                &field_types,
                &mut locals_cache,
            )
        };
        if let Some(payload) = payload {
            sites.push(RawSite {
                from_id,
                line,
                col,
                payload,
            });
        }
    }

    // 4b. Type-reference sites over each definition's signature. CodeGraph emits
    //     no type references for Python, C, or C++, so neither do we; Rust/TS,
    //     Go, and Java each contribute their signature type identifiers.
    if !matches!(lang, Language::Python | Language::C | Language::Cpp) {
        for (node, idx) in emitted {
            if !matches!(nodes[*idx].kind.as_str(), "function" | "method") {
                continue;
            }
            let from_id = &nodes[*idx].id;
            collect_type_refs(*node, src, lang, from_id, &mut sites);
        }
    }

    // 4c. Import sites: file → import-node edges. A plain Python `import X`
    //     (module alias, not a `from` import) gets no edge, matching CodeGraph.
    //     Java attaches its imports to the file's `namespace` (package) node.
    let namespace_id = (lang == Language::Java)
        .then(|| {
            emitted.iter().find_map(|(_, idx)| {
                (nodes[*idx].kind == "namespace").then(|| nodes[*idx].id.clone())
            })
        })
        .flatten();
    for (node, idx) in emitted {
        if nodes[*idx].kind != "import" {
            continue;
        }
        if lang == Language::Python && node.kind() == "import_statement" {
            continue;
        }
        let rec = &nodes[*idx];
        let from_id = namespace_id.clone().unwrap_or_else(|| file_id.to_string());
        sites.push(RawSite {
            from_id,
            line: rec.start_line,
            col: rec.start_column,
            payload: SitePayload::Import {
                target_id: rec.id.clone(),
                module: rec.name.clone(),
            },
        });
    }

    // 4d. Inheritance sites: extends (base class / supertrait / base interface)
    //     and implements (Rust `impl Trait for Type`, TS `class … implements`).
    collect_inheritance_sites(root, src, lang, emitted, nodes, type_nodes, &mut sites);

    sites
}

/// Collect `extends`/`implements` inheritance sites, matching CodeGraph 0.9.7's
/// conventions per language:
/// - **Rust**: a `trait Sub: Super` supertrait bound → `extends` (from the
///   trait); an `impl Trait for Type` block → `implements` (from the `Type`
///   struct/enum to the trait). Plain `impl Type {}` yields nothing.
/// - **Python**: every base in `class C(Base, …)` → `extends` (from the class).
/// - **TS/JS**: `class C extends Base` → `extends`; `class C implements I, …` →
///   `implements`. (CodeGraph 0.9.7 does not record `interface X extends Y`
///   inheritance — its clause walker misses `extends_type_clause` — so we don't
///   either, keeping edge parity exact.)
fn collect_inheritance_sites(
    root: Node,
    src: &str,
    lang: Language,
    emitted: &[(Node, usize)],
    nodes: &[NodeRecord],
    type_nodes: &HashMap<String, String>,
    sites: &mut Vec<RawSite>,
) {
    // Definition-anchored inheritance (class/trait heritage): source is the
    // definition's own node id.
    for (node, idx) in emitted {
        let rec = &nodes[*idx];
        let from_id = rec.id.clone();
        match lang {
            // Rust supertrait bounds: `trait Named: Shape + Send`.
            Language::Rust if rec.kind == "trait" => {
                if let Some(bounds) = child_of_kind(*node, "trait_bounds") {
                    for name in bound_type_names(bounds, src) {
                        push_inherit(sites, &from_id, bounds, "extends", name);
                    }
                }
            }
            // Python `class C(Base, …)` bases.
            Language::Python if rec.kind == "class" => {
                if let Some(supers) = node.child_by_field_name("superclasses") {
                    let mut c = supers.walk();
                    for arg in supers.named_children(&mut c) {
                        if matches!(arg.kind(), "identifier" | "attribute") {
                            let name = node_text(arg, src).to_string();
                            push_inherit(sites, &from_id, arg, "extends", name);
                        }
                    }
                }
            }
            // CodeGraph records TS/JS inheritance for classes only, via
            // class_heritage → extends_clause / implements_clause.
            Language::TypeScript | Language::Tsx | Language::JavaScript if rec.kind == "class" => {
                if let Some(heritage) = child_of_kind(*node, "class_heritage") {
                    let mut c = heritage.walk();
                    for clause in heritage.named_children(&mut c) {
                        let edge = match clause.kind() {
                            "extends_clause" => "extends",
                            "implements_clause" => "implements",
                            _ => continue,
                        };
                        for (nm, pos) in heritage_clause_types(clause, src) {
                            push_inherit(sites, &from_id, pos, edge, nm);
                        }
                    }
                }
            }
            // C++: every base in a `base_class_clause` → `extends` (CodeGraph
            // uses `extends` for all C++ bases; C++ has no `interface` kind, so
            // no `implements`). CodeGraph *also* fabricates a spurious
            // `class -> <method-return-type>` extends by misreading an in-class
            // method's return type as a base — we never emit that (it lives in
            // the class body, not the base clause), and it is whitelisted.
            Language::Cpp if matches!(rec.kind.as_str(), "class" | "struct") => {
                if let Some(bc) = child_of_kind(*node, "base_class_clause") {
                    for id in heritage_type_idents(bc, src) {
                        push_inherit(
                            sites,
                            &from_id,
                            id,
                            "extends",
                            node_text(id, src).to_string(),
                        );
                    }
                }
            }
            // Go: struct/interface embedding → `extends` (an embedded field/type
            // with no field name, or an embedded interface `type_elem`). Go has no
            // explicit `implements`.
            Language::Go => {
                if let Some(ty) = node.child_by_field_name("type") {
                    for id in go_embedded_type_idents(ty) {
                        push_inherit(
                            sites,
                            &from_id,
                            id,
                            "extends",
                            node_text(id, src).to_string(),
                        );
                    }
                }
            }
            // Java: a class's `superclass` (extends Base) → `extends`, its
            // `interfaces` (implements I, …) → `implements`; an interface's
            // `extends_interfaces` (extends I, …) → `extends`.
            Language::Java => match rec.kind.as_str() {
                "class" => {
                    if let Some(sc) = node.child_by_field_name("superclass") {
                        for id in heritage_type_idents(sc, src) {
                            push_inherit(
                                sites,
                                &from_id,
                                id,
                                "extends",
                                node_text(id, src).to_string(),
                            );
                        }
                    }
                    if let Some(ifs) = node.child_by_field_name("interfaces") {
                        for id in heritage_type_idents(ifs, src) {
                            push_inherit(
                                sites,
                                &from_id,
                                id,
                                "implements",
                                node_text(id, src).to_string(),
                            );
                        }
                    }
                }
                "interface" => {
                    if let Some(ext) = child_of_kind(*node, "extends_interfaces") {
                        for id in heritage_type_idents(ext, src) {
                            push_inherit(
                                sites,
                                &from_id,
                                id,
                                "extends",
                                node_text(id, src).to_string(),
                            );
                        }
                    }
                }
                _ => {}
            },
            // Non-class TS/JS, plain C (no inheritance), and tier-3 (handled by
            // tier3::extract) reach here.
            _ => {}
        }
    }

    // Rust `impl Trait for Type` → an `implements` edge from the implementing
    // type's node (found by name among the file's structs/enums) to the trait.
    if lang == Language::Rust {
        for impl_node in descendants_of_kind(root, "impl_item") {
            if let Some((type_name, trait_name, pos)) = rust_impl_for(impl_node, src) {
                if let Some(type_id) = type_nodes.get(&type_name) {
                    push_inherit(sites, type_id, pos, "implements", trait_name);
                }
            }
        }
    }
}

/// Push an `Inherit` site at `pos`'s coordinate.
fn push_inherit(
    sites: &mut Vec<RawSite>,
    from_id: &str,
    pos: Node,
    edge_kind: &'static str,
    parent: String,
) {
    let start = pos.start_position();
    sites.push(RawSite {
        from_id: from_id.to_string(),
        line: start.row as u32 + 1,
        col: start.column as u32,
        payload: SitePayload::Inherit { parent, edge_kind },
    });
}

/// The first direct child of `node` with kind `kind`.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut c = node.walk();
    let found = node.children(&mut c).find(|ch| ch.kind() == kind);
    found
}

/// Type names referenced by a Rust `trait_bounds` node (`type_identifier`s,
/// including the inner identifier of a `generic_type` bound like `Trait<'a>`).
fn bound_type_names(bounds: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut c = bounds.walk();
    for b in bounds.named_children(&mut c) {
        match b.kind() {
            "type_identifier" => out.push(node_text(b, src).to_string()),
            "generic_type" => {
                if let Some(inner) = child_of_kind(b, "type_identifier") {
                    out.push(node_text(inner, src).to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// The (name, position) of each heritage target in a TS `extends_clause` /
/// `implements_clause` — the leftmost identifier of each listed type.
fn heritage_clause_types<'a>(clause: Node<'a>, src: &str) -> Vec<(String, Node<'a>)> {
    let mut out = Vec::new();
    let mut c = clause.walk();
    for t in clause.named_children(&mut c) {
        // Skip type_arguments (`<T>`) that sit alongside the base in some grammars.
        if t.kind() == "type_arguments" {
            continue;
        }
        if let Some(name) = leftmost_identifier(t, src) {
            out.push((name, t));
        }
    }
    out
}

/// Every base-type `type_identifier` under a heritage / base-class clause, in
/// document order, skipping generic `type_arguments` subtrees (so `Foo<Bar>`
/// contributes only `Foo`, and `implements Shape, Named` yields both). Used for
/// C++ `base_class_clause` and Java `superclass`/`interfaces`/`extends_interfaces`,
/// whose listed types surface as `type_identifier` nodes (a namespace-qualified
/// C++ base like `geo::Base` still ends in a `type_identifier` `Base`, matching
/// CodeGraph's namespace-stripped resolution).
fn heritage_type_idents<'a>(clause: Node<'a>, _src: &str) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut stack = vec![clause];
    while let Some(n) = stack.pop() {
        // Generic arguments are not base types.
        if n.kind() == "type_arguments" {
            continue;
        }
        if n.kind() == "type_identifier" {
            out.push(n);
            continue;
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out.sort_by_key(|n| (n.start_byte(), n.end_byte()));
    out
}

/// The embedded-type `type_identifier`s of a Go `struct_type` / `interface_type`
/// (the language's "inheritance"): a struct `field_declaration` with a `type`
/// but no `name` field is an embedded field; an interface `type_elem` is an
/// embedded interface. A pointer/qualified/generic embed (`*Base`, `pkg.Base`)
/// still bottoms out at the base `type_identifier`. Named struct fields and
/// interface method elements are skipped.
fn go_embedded_type_idents<'a>(ty: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    match ty.kind() {
        "struct_type" => {
            if let Some(fdl) = child_of_kind(ty, "field_declaration_list") {
                let mut c = fdl.walk();
                for fd in fdl.named_children(&mut c) {
                    if fd.kind() != "field_declaration" || fd.child_by_field_name("name").is_some()
                    {
                        continue;
                    }
                    if let Some(t) = fd.child_by_field_name("type") {
                        if let Some(id) = go_base_type_id(t) {
                            out.push(id);
                        }
                    }
                }
            }
        }
        "interface_type" => {
            let mut c = ty.walk();
            for el in ty.named_children(&mut c) {
                if el.kind() == "type_elem" {
                    if let Some(id) = go_base_type_id(el) {
                        out.push(id);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// The base `type_identifier` of a Go embedded type expression (unwrapping a
/// `pointer_type`/`qualified_type`/`generic_type`/`type_elem` wrapper).
fn go_base_type_id(node: Node) -> Option<Node> {
    if node.kind() == "type_identifier" {
        return Some(node);
    }
    let mut c = node.walk();
    // Prefer a direct `type_identifier`; else recurse into the first child.
    for ch in node.named_children(&mut c) {
        if ch.kind() == "type_identifier" {
            return Some(ch);
        }
    }
    let first = node.named_child(0)?;
    go_base_type_id(first)
}

/// The leftmost identifier text of a type expression (`Base` from `Base`,
/// `ns.Base` → `ns`, `Base<T>` → `Base`).
fn leftmost_identifier(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" | "property_identifier" => {
            Some(node_text(node, src).to_string())
        }
        _ => {
            let first = node.named_child(0)?;
            leftmost_identifier(first, src)
        }
    }
}

/// For a Rust `impl_item`, if it is an `impl Trait for Type` block, return
/// `(type_name, trait_name, trait_position)`. Plain `impl Type {}` → `None`.
fn rust_impl_for<'a>(node: Node<'a>, src: &str) -> Option<(String, String, Node<'a>)> {
    let trait_node = node.child_by_field_name("trait")?;
    let type_node = node.child_by_field_name("type")?;
    let trait_name = last_path_segment(bare_type_text(trait_node, src));
    let type_name = last_path_segment(bare_type_text(type_node, src));
    if trait_name.is_empty() || type_name.is_empty() {
        None
    } else {
        Some((type_name, trait_name, trait_node))
    }
}

/// The base type text with any generic argument list stripped (`Point<T>` →
/// `Point`, `geometry::Point` kept for the caller's `last_path_segment`).
fn bare_type_text<'a>(node: Node<'a>, src: &'a str) -> &'a str {
    let txt = node_text(node, src);
    txt.split('<').next().unwrap_or(txt).trim()
}

/// All descendants of `root` (inclusive) whose kind is `kind`, in document order.
fn descendants_of_kind<'a>(root: Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == kind {
            out.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out.sort_by_key(|n| (n.start_byte(), n.end_byte()));
    out
}

/// Derive a call/method/constructor payload from a captured call node.
#[allow(clippy::too_many_arguments)]
fn call_payload(
    call: Node,
    src: &str,
    lang: Language,
    from_id: &str,
    def_node_by_id: &HashMap<String, Node>,
    encl_type_by_id: &HashMap<String, String>,
    field_types: &HashMap<String, HashMap<String, String>>,
    locals_cache: &mut HashMap<String, HashMap<String, RecvExpr>>,
) -> Option<SitePayload> {
    // Java call sites are `method_invocation` nodes (no `function` field).
    if lang == Language::Java {
        return java_call_payload(
            call,
            src,
            from_id,
            def_node_by_id,
            encl_type_by_id,
            field_types,
            locals_cache,
        );
    }
    let func = call.child_by_field_name("function")?;
    match lang {
        Language::C => match func.kind() {
            "identifier" => Some(SitePayload::CallOrCtor {
                name: node_text(func, src).to_string(),
            }),
            _ => None,
        },
        Language::Cpp => match func.kind() {
            "identifier" => Some(SitePayload::CallOrCtor {
                name: node_text(func, src).to_string(),
            }),
            // Member call `recv.m(...)` / `recv->m(...)`: resolve the method by
            // its (unique) name — 0.9.7 does not type the C++ receiver.
            "field_expression" => {
                field_text(func, "field", src).map(|name| SitePayload::CallOrCtor { name })
            }
            // `ns::func(...)` qualified call → resolves against its qualified name.
            "qualified_identifier" => {
                let name = field_text(func, "name", src)?;
                let scope = func.child_by_field_name("scope")?;
                Some(SitePayload::QualifiedCall {
                    qualifier: last_scope_segment(node_text(scope, src)),
                    name,
                })
            }
            _ => None,
        },
        Language::Go => match func.kind() {
            "identifier" => Some(SitePayload::CallOrCtor {
                name: node_text(func, src).to_string(),
            }),
            // Selector call `recv.M(...)` (or `pkg.F(...)`): resolve by method
            // name; a package function that is not a graph symbol simply drops.
            "selector_expression" => {
                field_text(func, "field", src).map(|name| SitePayload::CallOrCtor { name })
            }
            _ => None,
        },
        Language::Java => None, // handled above
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => match func.kind() {
            "identifier" => Some(SitePayload::CallOrCtor {
                name: node_text(func, src).to_string(),
            }),
            "scoped_identifier" => {
                let name = field_text(func, "name", src)?;
                let path = func.child_by_field_name("path")?;
                let qualifier = last_path_segment(node_text(path, src));
                Some(SitePayload::QualifiedCall { qualifier, name })
            }
            "field_expression" => {
                let name = field_text(func, "field", src)?;
                let recv = func.child_by_field_name("value")?;
                let recv = recv_of(
                    recv,
                    src,
                    from_id,
                    def_node_by_id,
                    encl_type_by_id,
                    field_types,
                    locals_cache,
                    lang,
                );
                Some(SitePayload::MethodCall { recv, name })
            }
            _ => None,
        },
        Language::Python => match func.kind() {
            "identifier" => Some(SitePayload::CallOrCtor {
                name: node_text(func, src).to_string(),
            }),
            "attribute" => {
                let name = field_text(func, "attribute", src)?;
                let recv = func.child_by_field_name("object")?;
                let recv = recv_of(
                    recv,
                    src,
                    from_id,
                    def_node_by_id,
                    encl_type_by_id,
                    field_types,
                    locals_cache,
                    lang,
                );
                Some(SitePayload::MethodCall { recv, name })
            }
            _ => None,
        },
        Language::TypeScript | Language::Tsx | Language::JavaScript => match func.kind() {
            "identifier" => Some(SitePayload::CallOrCtor {
                name: node_text(func, src).to_string(),
            }),
            "member_expression" => {
                let name = field_text(func, "property", src)?;
                let recv = func.child_by_field_name("object")?;
                let recv = recv_of(
                    recv,
                    src,
                    from_id,
                    def_node_by_id,
                    encl_type_by_id,
                    field_types,
                    locals_cache,
                    lang,
                );
                Some(SitePayload::MethodCall { recv, name })
            }
            _ => None,
        },
    }
}

/// Derive the class name a construction expression builds — a `new X(...)`
/// (TS/JS `new_expression`, Java `object_creation_expression`, C++
/// `new_expression`). Resolves to an `instantiates` edge to the class node.
fn new_payload(new_expr: Node, src: &str, lang: Language) -> Option<SitePayload> {
    match lang {
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            let ctor = new_expr.child_by_field_name("constructor")?;
            (ctor.kind() == "identifier").then(|| SitePayload::New {
                name: node_text(ctor, src).to_string(),
            })
        }
        // Java `new Point(...)` and C++ `new geo::Point(...)` name the class in a
        // `type:` field; a namespaced C++ type keeps only its last segment.
        Language::Java | Language::Cpp => {
            let ty = new_expr.child_by_field_name("type")?;
            let name = last_scope_segment(node_text(ty, src));
            (!name.is_empty()).then_some(SitePayload::New { name })
        }
        _ => None,
    }
}

/// Derive a Java `method_invocation` payload:
/// - unqualified `method(...)` → resolve by name (`CallOrCtor`);
/// - `Class.method(...)` (receiver is a class-name identifier) → a qualified call
///   against `package::Class::method`;
/// - `recv.method(...)` where the receiver's type is locally inferable (a typed
///   parameter/local, or `this`) → a qualified call against that type's method;
/// - any other receiver (array element, chained call) → dropped.
#[allow(clippy::too_many_arguments)]
fn java_call_payload(
    call: Node,
    src: &str,
    from_id: &str,
    def_node_by_id: &HashMap<String, Node>,
    encl_type_by_id: &HashMap<String, String>,
    field_types: &HashMap<String, HashMap<String, String>>,
    locals_cache: &mut HashMap<String, HashMap<String, RecvExpr>>,
) -> Option<SitePayload> {
    let name = field_text(call, "name", src)?;
    let Some(obj) = call.child_by_field_name("object") else {
        return Some(SitePayload::CallOrCtor { name });
    };
    let pkg = java_package(call, src);
    // A typed local/parameter receiver (or `this`) resolves via its type. Java
    // locals are explicitly typed, so only a concrete `Type` receiver is used
    // (Java doesn't use the return-type/chained inference tier-1 languages do).
    if let Some(RecvExpr::Type(ty)) = recv_of(
        obj,
        src,
        from_id,
        def_node_by_id,
        encl_type_by_id,
        field_types,
        locals_cache,
        Language::Java,
    ) {
        return Some(SitePayload::QualifiedCall {
            qualifier: qualify_java_type(&ty, pkg.as_deref()),
            name,
        });
    }
    // A bare class-name identifier is a static `Class.method(...)` call.
    if obj.kind() == "identifier" && starts_uppercase(node_text(obj, src)) {
        return Some(SitePayload::QualifiedCall {
            qualifier: qualify_java_type(node_text(obj, src), pkg.as_deref()),
            name,
        });
    }
    None
}

/// Package-qualify a Java simple type name (`Point` → `fixture::Point`); an
/// already-qualified type (`fixture::Point`, e.g. from `this`) is left as-is.
fn qualify_java_type(ty: &str, pkg: Option<&str>) -> String {
    match pkg {
        Some(p) if !ty.contains("::") => format!("{p}::{ty}"),
        _ => ty.to_string(),
    }
}

fn starts_uppercase(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Describe a method-call receiver as a [`RecvExpr`] (a concrete type, or a call
/// whose return type will type it at resolve time), memoizing the enclosing
/// callable's local-variable types in `locals_cache`. `None` when the receiver
/// can't be typed from evidence (then the method call is dropped).
#[allow(clippy::too_many_arguments)]
fn recv_of(
    recv: Node,
    src: &str,
    from_id: &str,
    def_node_by_id: &HashMap<String, Node>,
    encl_type_by_id: &HashMap<String, String>,
    field_types: &HashMap<String, HashMap<String, String>>,
    locals_cache: &mut HashMap<String, HashMap<String, RecvExpr>>,
    lang: Language,
) -> Option<RecvExpr> {
    let ctx = RecvCtx {
        src,
        lang,
        encl_type: encl_type_by_id.get(from_id).map(|s| s.as_str()),
        field_types,
    };
    if !locals_cache.contains_key(from_id) {
        let locals = def_node_by_id
            .get(from_id)
            .map(|n| ctx.infer_locals(*n))
            .unwrap_or_default();
        locals_cache.insert(from_id.to_string(), locals);
    }
    let locals = locals_cache.get(from_id)?;
    ctx.recv_expr(recv, locals)
}

/// Immutable context for typing receivers within one enclosing callable.
struct RecvCtx<'a> {
    src: &'a str,
    lang: Language,
    /// The enclosing type, if the callable is a method (for `self`/`this`).
    encl_type: Option<&'a str>,
    field_types: &'a HashMap<String, HashMap<String, String>>,
}

impl RecvCtx<'_> {
    /// Type a receiver expression from `locals` (a partial map while building, or
    /// the complete map at a call site).
    fn recv_expr(&self, recv: Node, locals: &HashMap<String, RecvExpr>) -> Option<RecvExpr> {
        // `self`/`this` → the enclosing type.
        if matches!(recv.kind(), "self" | "this") {
            return self.encl_type.map(|t| RecvExpr::Type(t.to_string()));
        }
        // `self.field` / `this.field` → the enclosing type's declared field type.
        if let Some(field) = self_field_name(recv, self.src, self.lang) {
            let encl = self.encl_type?;
            return self
                .field_types
                .get(encl)?
                .get(&field)
                .cloned()
                .map(RecvExpr::Type);
        }
        match recv.kind() {
            "identifier" => {
                let name = node_text(recv, self.src);
                if matches!(name, "self" | "this") {
                    return self.encl_type.map(|t| RecvExpr::Type(t.to_string()));
                }
                locals.get(name).cloned()
            }
            // A chained receiver `a.b()` — its type is that call's return type.
            "call_expression" | "call" => {
                Some(RecvExpr::Return(Box::new(self.call_ref(recv, locals)?)))
            }
            // Transparent Rust wrappers around the real receiver expression.
            "try_expression"
            | "await_expression"
            | "parenthesized_expression"
            | "reference_expression" => self.recv_expr(recv.named_child(0)?, locals),
            _ => None,
        }
    }

    /// Describe a call node as a [`CallRef`] (bare / qualified / method), so its
    /// return type can be resolved in pass 2. `None` for a callee shape we can't
    /// resolve (e.g. a call through an index or an un-typed receiver).
    fn call_ref(&self, call: Node, locals: &HashMap<String, RecvExpr>) -> Option<CallRef> {
        let func = call.child_by_field_name("function")?;
        match self.lang {
            Language::Rust => match func.kind() {
                "identifier" => Some(CallRef::Bare {
                    name: node_text(func, self.src).to_string(),
                }),
                "scoped_identifier" => {
                    let name = field_text(func, "name", self.src)?;
                    let path = func.child_by_field_name("path")?;
                    Some(CallRef::Qualified {
                        qualifier: last_path_segment(node_text(path, self.src)),
                        name,
                    })
                }
                "field_expression" => {
                    let name = field_text(func, "field", self.src)?;
                    let recv = self.recv_expr(func.child_by_field_name("value")?, locals)?;
                    Some(CallRef::Method { recv, name })
                }
                _ => None,
            },
            Language::Python => match func.kind() {
                "identifier" => Some(CallRef::Bare {
                    name: node_text(func, self.src).to_string(),
                }),
                "attribute" => {
                    let name = field_text(func, "attribute", self.src)?;
                    let recv = self.recv_expr(func.child_by_field_name("object")?, locals)?;
                    Some(CallRef::Method { recv, name })
                }
                _ => None,
            },
            Language::TypeScript | Language::Tsx | Language::JavaScript => match func.kind() {
                "identifier" => Some(CallRef::Bare {
                    name: node_text(func, self.src).to_string(),
                }),
                "member_expression" => {
                    let name = field_text(func, "property", self.src)?;
                    let recv = self.recv_expr(func.child_by_field_name("object")?, locals)?;
                    Some(CallRef::Method { recv, name })
                }
                _ => None,
            },
            // C/C++/Go/Java resolve method calls by name / explicit types, not
            // through this return-type chain.
            _ => None,
        }
    }

    /// Build the `var → RecvExpr` map for one callable: typed parameters, `self`,
    /// and locals bound by a `let`/assignment (to a constructor → its type, or to
    /// a call → that call's return type). A single document-order pass, so an
    /// earlier local is available to a later one.
    fn infer_locals(&self, func: Node) -> HashMap<String, RecvExpr> {
        let mut locals: HashMap<String, RecvExpr> = HashMap::new();
        if let Some(t) = self.encl_type {
            locals.insert("self".to_string(), RecvExpr::Type(t.to_string()));
            locals.insert("this".to_string(), RecvExpr::Type(t.to_string()));
        }
        if let Some(params) = func.child_by_field_name("parameters") {
            let mut c = params.walk();
            for p in params.named_children(&mut c) {
                self.infer_param(p, &mut locals);
            }
        }
        if let Some(body) = func.child_by_field_name("body") {
            self.walk_assignments(body, &mut locals);
        }
        locals
    }

    fn infer_param(&self, p: Node, locals: &mut HashMap<String, RecvExpr>) {
        let mut put = |name: &str, ty: &str| {
            if let Some(t) = bare_type_name(ty) {
                locals.insert(name.to_string(), RecvExpr::Type(t));
            }
        };
        match self.lang {
            Language::Rust => {
                if p.kind() == "self_parameter" {
                    if let Some(t) = self.encl_type {
                        locals.insert("self".to_string(), RecvExpr::Type(t.to_string()));
                    }
                } else if p.kind() == "parameter" {
                    if let (Some(pat), Some(ty)) = (
                        p.child_by_field_name("pattern"),
                        p.child_by_field_name("type"),
                    ) {
                        if pat.kind() == "identifier" {
                            put(node_text(pat, self.src), node_text(ty, self.src));
                        }
                    }
                }
            }
            Language::Python => {
                if p.kind() == "typed_parameter" {
                    let mut c = p.walk();
                    let name = p.named_children(&mut c).find(|n| n.kind() == "identifier");
                    if let (Some(name), Some(ty)) = (name, p.child_by_field_name("type")) {
                        put(node_text(name, self.src), node_text(ty, self.src));
                    }
                }
            }
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                if matches!(p.kind(), "required_parameter" | "optional_parameter") {
                    if let (Some(pat), Some(ann)) = (
                        p.child_by_field_name("pattern"),
                        p.child_by_field_name("type"),
                    ) {
                        if pat.kind() == "identifier" {
                            let inner = ann.named_child(0).unwrap_or(ann);
                            put(node_text(pat, self.src), node_text(inner, self.src));
                        }
                    }
                }
            }
            Language::Java => {
                if p.kind() == "formal_parameter" {
                    if let (Some(ty), Some(nm)) =
                        (p.child_by_field_name("type"), p.child_by_field_name("name"))
                    {
                        put(node_text(nm, self.src), node_text(ty, self.src));
                    }
                }
            }
            Language::C | Language::Cpp | Language::Go => {}
            Language::Ruby
            | Language::Php
            | Language::Kotlin
            | Language::Swift
            | Language::Scala
            | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        }
    }

    /// Record `var → RecvExpr` for each `let`/assignment in a callable body: a
    /// constructor value → its concrete type; a call value → that call's return
    /// type ([`RecvExpr::Return`], resolved in pass 2). Recurses over the body.
    fn walk_assignments(&self, node: Node, locals: &mut HashMap<String, RecvExpr>) {
        // Loop variable over a call's collection: `for x in reg.list() { x.m() }`
        // types `x` as the collection's element type.
        if let Some((var, call)) = self.for_binding(node) {
            if let Some(cref) = self.call_ref(call, locals) {
                locals.insert(var, RecvExpr::Element(Box::new(cref)));
            }
        }
        match self.lang {
            Language::Rust => {
                if node.kind() == "let_declaration" {
                    if let (Some(pat), Some(val)) = (
                        node.child_by_field_name("pattern"),
                        node.child_by_field_name("value"),
                    ) {
                        if pat.kind() == "identifier" {
                            if let Some(re) = self.value_type(val, locals) {
                                locals.insert(node_text(pat, self.src).to_string(), re);
                            }
                        }
                    }
                }
            }
            Language::Python => {
                if node.kind() == "assignment" {
                    // Prefer an explicit annotation (`x: T = …`); else the RHS value.
                    if let Some(left) = node.child_by_field_name("left") {
                        if left.kind() == "identifier" {
                            let re = node
                                .child_by_field_name("type")
                                .and_then(|ty| bare_type_name(node_text(ty, self.src)))
                                .map(RecvExpr::Type)
                                .or_else(|| {
                                    node.child_by_field_name("right")
                                        .and_then(|v| self.value_type(v, locals))
                                });
                            if let Some(re) = re {
                                locals.insert(node_text(left, self.src).to_string(), re);
                            }
                        }
                    }
                }
            }
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                if node.kind() == "variable_declarator" {
                    if let (Some(name), Some(val)) = (
                        node.child_by_field_name("name"),
                        node.child_by_field_name("value"),
                    ) {
                        if name.kind() == "identifier" {
                            // An explicit `: T` annotation wins over the initializer.
                            let re = node
                                .child_by_field_name("type")
                                .map(|ann| ann.named_child(0).unwrap_or(ann))
                                .and_then(|t| bare_type_name(node_text(t, self.src)))
                                .map(RecvExpr::Type)
                                .or_else(|| self.value_type(val, locals));
                            if let Some(re) = re {
                                locals.insert(node_text(name, self.src).to_string(), re);
                            }
                        }
                    }
                }
            }
            Language::Java => {
                if node.kind() == "local_variable_declaration" {
                    if let Some(ty) = node.child_by_field_name("type") {
                        if let Some(t) = bare_type_name(node_text(ty, self.src)) {
                            let mut c = node.walk();
                            for d in node.named_children(&mut c) {
                                if d.kind() == "variable_declarator" {
                                    if let Some(nm) = d.child_by_field_name("name") {
                                        locals.insert(
                                            node_text(nm, self.src).to_string(),
                                            RecvExpr::Type(t.clone()),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Language::C | Language::Cpp | Language::Go => {}
            Language::Ruby
            | Language::Php
            | Language::Kotlin
            | Language::Swift
            | Language::Scala
            | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            self.walk_assignments(child, locals);
        }
    }

    /// The [`RecvExpr`] a right-hand-side *value* produces: a Rust struct literal
    /// or a construction → its concrete type; a call → that call's return type; a
    /// transparent wrapper (`v?`, `v.await`, `(v)`) → its inner value.
    fn value_type(&self, val: Node, locals: &HashMap<String, RecvExpr>) -> Option<RecvExpr> {
        match val.kind() {
            "try_expression"
            | "await_expression"
            | "parenthesized_expression"
            | "reference_expression" => self.value_type(val.named_child(0)?, locals),
            "struct_expression" => {
                let name = val.child_by_field_name("name")?;
                Some(RecvExpr::Type(last_path_segment(node_text(name, self.src))))
            }
            "new_expression" => {
                let ctor = val.child_by_field_name("constructor")?;
                (ctor.kind() == "identifier")
                    .then(|| RecvExpr::Type(node_text(ctor, self.src).to_string()))
            }
            "call_expression" | "call" => {
                Some(RecvExpr::Return(Box::new(self.call_ref(val, locals)?)))
            }
            _ => None,
        }
    }

    /// If `node` is a `for x in <call>` loop (Rust/Python) whose loop variable is
    /// a plain identifier and whose iterable is a *call* (after stripping a Rust
    /// `&`/`&mut`), return `(var, call_node)`. The element type of the call's
    /// collection return type will type the loop variable. Iterating a bare local
    /// (`for x in &v`) is not handled — no collection-element evidence.
    fn for_binding<'t>(&self, node: Node<'t>) -> Option<(String, Node<'t>)> {
        let (pat_field, val_field, kind) = match self.lang {
            Language::Rust => ("pattern", "value", "for_expression"),
            Language::Python => ("left", "right", "for_statement"),
            _ => return None,
        };
        if node.kind() != kind {
            return None;
        }
        let pat = node.child_by_field_name(pat_field)?;
        if pat.kind() != "identifier" {
            return None;
        }
        let mut val = node.child_by_field_name(val_field)?;
        // Rust `for x in &coll` / `&mut coll` — the call, if any, is inside.
        if val.kind() == "reference_expression" {
            val = val.named_child(val.named_child_count().checked_sub(1)?)?;
        }
        if matches!(val.kind(), "call_expression" | "call") {
            Some((node_text(pat, self.src).to_string(), val))
        } else {
            None
        }
    }
}

/// The last `::`-segment of a Rust path (`geometry::Point` → `Point`).
fn last_path_segment(path: &str) -> String {
    path.rsplit("::").next().unwrap_or(path).trim().to_string()
}

/// If `recv` is a `self.field` / `this.field` access (a field/member/attribute
/// expression whose object is `self`/`this`), return the field's name.
fn self_field_name(recv: Node, src: &str, lang: Language) -> Option<String> {
    // Only tier-1 languages get `self.field` typing (the field-type map is built
    // for them). Other packs fall through to their own receiver handling.
    let (obj_field, name_field, expected_kind) = match lang {
        Language::Rust => ("value", "field", "field_expression"),
        Language::Python => ("object", "attribute", "attribute"),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            ("object", "property", "member_expression")
        }
        _ => return None,
    };
    if recv.kind() != expected_kind {
        return None;
    }
    let obj = recv.child_by_field_name(obj_field)?;
    let obj_txt = node_text(obj, src);
    if !matches!(obj.kind(), "self" | "this") && !matches!(obj_txt, "self" | "this") {
        return None;
    }
    let field = recv.child_by_field_name(name_field)?;
    if matches!(
        field.kind(),
        "field_identifier" | "property_identifier" | "identifier"
    ) {
        Some(node_text(field, src).to_string())
    } else {
        None
    }
}

/// Build a `type name → (field name → field type name)` map for one file, so a
/// `self.field.method()` receiver can be typed from the type's declared fields.
/// Covers Rust struct fields and TS class field annotations (Python class fields
/// are rarely type-annotated at class scope, and CodeGraph emits no Python type
/// references, so they are skipped).
fn collect_field_types(
    root: Node,
    src: &str,
    lang: Language,
) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    match lang {
        Language::Rust => {
            for st in descendants_of_kind(root, "struct_item") {
                let Some(name) = field_text(st, "name", src) else {
                    continue;
                };
                let Some(body) = st.child_by_field_name("body") else {
                    continue;
                };
                let mut c = body.walk();
                for f in body.named_children(&mut c) {
                    if f.kind() != "field_declaration" {
                        continue;
                    }
                    if let (Some(fname), Some(fty)) =
                        (f.child_by_field_name("name"), f.child_by_field_name("type"))
                    {
                        if let Some(t) = bare_type_name(node_text(fty, src)) {
                            out.entry(name.clone())
                                .or_default()
                                .insert(node_text(fname, src).to_string(), t);
                        }
                    }
                }
            }
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            for cls in descendants_of_kind(root, "class_declaration")
                .into_iter()
                .chain(descendants_of_kind(root, "abstract_class_declaration"))
            {
                let Some(name) = field_text(cls, "name", src) else {
                    continue;
                };
                let Some(body) = cls.child_by_field_name("body") else {
                    continue;
                };
                let mut c = body.walk();
                for m in body.named_children(&mut c) {
                    if m.kind() != "public_field_definition" {
                        continue;
                    }
                    if let (Some(fname), Some(ann)) =
                        (m.child_by_field_name("name"), m.child_by_field_name("type"))
                    {
                        // `type` is a `type_annotation` (`: T`); read its inner type.
                        let inner = ann.named_child(0).unwrap_or(ann);
                        if let Some(t) = bare_type_name(node_text(inner, src)) {
                            out.entry(name.clone())
                                .or_default()
                                .insert(node_text(fname, src).to_string(), t);
                        }
                    }
                }
            }
        }
        // Python class fields are rarely type-annotated at class scope, and the
        // tier-2/tier-3 packs do not use this map, so no field types are collected.
        _ => {}
    }
    out
}

/// Emit a `TypeRef` site for every type identifier appearing in a definition's
/// signature (parameters + return type) or field annotation, in document order.
fn collect_type_refs(
    node: Node,
    src: &str,
    lang: Language,
    from_id: &str,
    sites: &mut Vec<RawSite>,
) {
    // The subtree(s) that constitute the "signature" for this definition kind.
    let mut regions: Vec<Node> = Vec::new();
    // Go/Java resolve type references *strictly* to concrete types (struct /
    // interface / class / enum), excluding type aliases — matching CodeGraph,
    // which never references a Go alias like `Scalar`.
    let mut strict = false;
    match lang {
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust | Language::TypeScript | Language::Tsx | Language::JavaScript => {
            if node.kind() == "public_field_definition" {
                // TS class field: the `: T` annotation.
                if let Some(t) = node.child_by_field_name("type") {
                    regions.push(t);
                }
            } else {
                if let Some(p) = node.child_by_field_name("parameters") {
                    regions.push(p);
                }
                if let Some(r) = node.child_by_field_name("return_type") {
                    regions.push(r);
                }
            }
        }
        Language::Go => {
            strict = true;
            // Parameters + result type; the receiver is a separate field, so it
            // is excluded (CodeGraph does not reference the receiver type).
            if let Some(p) = node.child_by_field_name("parameters") {
                regions.push(p);
            }
            if let Some(r) = node.child_by_field_name("result") {
                regions.push(r);
            }
        }
        Language::Java => {
            strict = true;
            // Parameter list + return type.
            if let Some(p) = node.child_by_field_name("parameters") {
                regions.push(p);
            }
            if let Some(r) = node.child_by_field_name("type") {
                regions.push(r);
            }
        }
        Language::Python | Language::C | Language::Cpp => {}
    }
    for region in regions {
        collect_type_identifiers(region, src, from_id, strict, sites);
    }
}

/// Recursively collect `type_identifier` nodes under `region`, one `TypeRef`
/// (or `TypeRefStrict`, excluding aliases) site each, at the identifier's own
/// position.
fn collect_type_identifiers(
    region: Node,
    src: &str,
    from_id: &str,
    strict: bool,
    sites: &mut Vec<RawSite>,
) {
    let mut stack = vec![region];
    let mut found: Vec<Node> = Vec::new();
    while let Some(n) = stack.pop() {
        // Java uses `type_identifier`; Go type identifiers also surface as
        // `type_identifier` in its grammar.
        if n.kind() == "type_identifier" {
            found.push(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    // Document order (the stack walk is not ordered).
    found.sort_by_key(|n| (n.start_byte(), n.end_byte()));
    for id in found {
        let start = id.start_position();
        let name = node_text(id, src).to_string();
        let payload = if strict {
            SitePayload::TypeRefStrict { name }
        } else {
            SitePayload::TypeRef { name }
        };
        sites.push(RawSite {
            from_id: from_id.to_string(),
            line: start.row as u32 + 1,
            col: start.column as u32,
            payload,
        });
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

/// A [`FileExtract`] for a file that could not be read as UTF-8 (or at all):
/// a single `file` node plus a `files` row whose `errors` column is populated,
/// so the file is *recorded* rather than silently dropped — matching CodeGraph,
/// which writes an errored `files` row instead of omitting the file.
pub fn error_extract(
    stored_path: &str,
    bytes: &[u8],
    language: Language,
    mtime_millis: i64,
    now_millis: i64,
    message: &str,
) -> FileExtract {
    let db_lang = language.db_name();
    let line_count = String::from_utf8_lossy(bytes).split('\n').count().max(1) as u32;
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
    let file_record = FileRecord {
        path: stored_path.to_string(),
        content_hash: content_hash(bytes),
        language: db_lang.to_string(),
        size: bytes.len() as u64,
        modified_at: mtime_millis,
        indexed_at: now_millis,
        node_count: 1,
        errors: serde_json::to_string(&[message]).ok(),
    };
    FileExtract {
        file_record,
        nodes: vec![file_node],
        edges: Vec::new(),
        sites: Vec::new(),
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
    // Tier-2 languages derive several names from declarators/specs rather than a
    // plain `name:` field.
    match lang {
        Language::C | Language::Cpp if matches!(kind, "function" | "method") => {
            return cpp_declarator_id(node).map(|id| last_scope_segment(node_text(id, src)));
        }
        // A C `typedef double Scalar;` names the alias in its `declarator`
        // (`type_identifier`); C++ `using X = …;` uses a `name:` field.
        Language::C | Language::Cpp if kind == "type_alias" => {
            return field_text(node, "name", src).or_else(|| field_text(node, "declarator", src));
        }
        Language::Go if matches!(kind, "constant" | "variable") => {
            return go_spec_name(node, src);
        }
        Language::Java if kind == "namespace" => {
            return java_package_of_decl(node, src);
        }
        Language::Java if kind == "field" => {
            return java_field_name(node, src);
        }
        _ => {}
    }
    match kind {
        "import" => import_name(node, src, lang),
        "variable" => field_text(node, "left", src).or_else(|| field_text(node, "name", src)),
        "enum_member" => {
            field_text(node, "name", src).or_else(|| Some(node_text(node, src).to_string()))
        }
        _ => field_text(node, "name", src),
    }
}

/// The last `::`-scope segment of a C++ (qualified) identifier
/// (`geo::Point::distanceTo` → `distanceTo`, `hypot_scalar` → `hypot_scalar`).
fn last_scope_segment(text: &str) -> String {
    text.rsplit("::").next().unwrap_or(text).trim().to_string()
}

/// The innermost identifier node of a C/C++ `function_definition`'s declarator
/// (an `identifier`, `field_identifier`, or `qualified_identifier`), reached by
/// following the `declarator` field to its end.
fn cpp_declarator_id(node: Node) -> Option<Node> {
    let mut cur = node.child_by_field_name("declarator")?;
    while let Some(inner) = cur.child_by_field_name("declarator") {
        cur = inner;
    }
    Some(cur)
}

/// The first name of a Go `const_spec` / `var_spec` (its leading `identifier`).
fn go_spec_name(node: Node, src: &str) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some(node_text(n, src).to_string());
    }
    let mut c = node.walk();
    let found = node
        .named_children(&mut c)
        .find(|n| n.kind() == "identifier");
    found.map(|n| node_text(n, src).to_string())
}

/// The variable name of a Java `field_declaration` (its declarator's `name:`).
fn java_field_name(node: Node, src: &str) -> Option<String> {
    let mut c = node.walk();
    let decl = node
        .named_children(&mut c)
        .find(|n| n.kind() == "variable_declarator")?;
    decl.child_by_field_name("name")
        .map(|n| node_text(n, src).to_string())
}

/// The dotted package name of a Java `package_declaration` node.
fn java_package_of_decl(node: Node, src: &str) -> Option<String> {
    let mut c = node.walk();
    let found = node
        .named_children(&mut c)
        .find(|n| matches!(n.kind(), "scoped_identifier" | "identifier"));
    found.map(|n| node_text(n, src).to_string())
}

/// Reclassify a captured node to its final kind:
/// - a `function` nested in a type/impl/class body (or, in C++, defined with a
///   `Type::member` qualified declarator) becomes a `method`;
/// - a Go `type_alias` capture is refined to `struct`/`interface`/`type_alias`
///   by its right-hand side.
fn reclassify(kind: &str, node: Node, lang: Language) -> String {
    if lang == Language::Go && kind == "type_alias" {
        return go_type_kind(node).to_string();
    }
    if kind == "function" {
        if lang == Language::Cpp {
            // A C++ definition whose declarator name is qualified (`Point::m`) is
            // an out-of-line method; a plain identifier is a free function.
            if let Some(id) = cpp_declarator_id(node) {
                if id.kind() == "qualified_identifier" {
                    return "method".to_string();
                }
            }
            return "function".to_string();
        }
        if has_method_container(node, lang) {
            return "method".to_string();
        }
    }
    kind.to_string()
}

/// Classify a Go `type_spec`/`type_alias` capture by its right-hand side:
/// a `struct_type` → `struct`, an `interface_type` → `interface`, else
/// `type_alias` (`type Kind int`, `type Scalar = float64`).
fn go_type_kind(node: Node) -> &'static str {
    if let Some(ty) = node.child_by_field_name("type") {
        match ty.kind() {
            "struct_type" => return "struct",
            "interface_type" => return "interface",
            _ => {}
        }
    }
    "type_alias"
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
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => matches!(kind, "impl_item" | "trait_item"),
        Language::Python => kind == "class_definition",
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            matches!(kind, "class_declaration" | "abstract_class_declaration")
        }
        // C++ methods are reclassified by their qualified declarator, not by
        // container nesting; the other tier-2 languages capture methods directly.
        Language::C | Language::Cpp | Language::Go | Language::Java => false,
    }
}

/// Build the `::`-joined qualified name by walking enclosing type containers.
fn qualified_name(node: Node, name: &str, src: &str, lang: Language) -> String {
    // Method qualification that does not come from ancestor nesting:
    // C++ out-of-line methods carry a `Type::method` declarator, and Go methods
    // are qualified by their receiver type.
    match lang {
        Language::Cpp => {
            if let Some(qn) = cpp_method_qn(node, src) {
                return qn;
            }
        }
        Language::Go if node.kind() == "method_declaration" => {
            if let Some(recv) = go_receiver_type(node, src) {
                return format!("{recv}::{name}");
            }
        }
        _ => {}
    }

    let mut parts = Vec::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        if let Some(container) = qual_container_name(n, src, lang) {
            parts.push(container);
        }
        cur = n.parent();
    }
    parts.reverse();
    // Java qualifies every declaration by its file's package (but the package
    // node itself is named by the package, not `package::package`).
    if lang == Language::Java && node.kind() != "package_declaration" {
        if let Some(pkg) = java_package(node, src) {
            parts.insert(0, pkg);
        }
    }
    if parts.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", parts.join("::"), name)
    }
}

/// The `Type::method` qualified name of a C++ out-of-line method definition, or
/// `None` when the declarator is a plain identifier (a free function).
fn cpp_method_qn(node: Node, src: &str) -> Option<String> {
    if node.kind() != "function_definition" {
        return None;
    }
    let id = cpp_declarator_id(node)?;
    (id.kind() == "qualified_identifier").then(|| node_text(id, src).trim().to_string())
}

/// The base type name of a Go method's receiver (`(p *Point)` / `(p Point)` →
/// `Point`).
fn go_receiver_type(node: Node, src: &str) -> Option<String> {
    let recv = node.child_by_field_name("receiver")?;
    let mut c = recv.walk();
    let param = recv
        .named_children(&mut c)
        .find(|n| n.kind() == "parameter_declaration")?;
    let ty = param.child_by_field_name("type")?;
    bare_type_name(node_text(ty, src))
}

/// The dotted package name in effect for `node`'s file (the top-level
/// `package_declaration`), if any.
fn java_package(node: Node, src: &str) -> Option<String> {
    let mut root = node;
    while let Some(p) = root.parent() {
        root = p;
    }
    let mut c = root.walk();
    let pkg = root
        .named_children(&mut c)
        .find(|n| n.kind() == "package_declaration")?;
    java_package_of_decl(pkg, src)
}

fn qual_container_name(node: Node, src: &str, lang: Language) -> Option<String> {
    match lang {
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
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
        // C/C++ qualify only enum members by their enum (`Shape::Circle`);
        // namespaces are ignored and methods carry their own qualifier.
        Language::C | Language::Cpp => match node.kind() {
            "enum_specifier" => field_text(node, "name", src),
            _ => None,
        },
        // Go qualifies nothing by nesting (methods use their receiver).
        Language::Go => None,
        // Java qualifies members by their enclosing class/interface/enum (the
        // package prefix is added separately).
        Language::Java => match node.kind() {
            "class_declaration" | "interface_declaration" | "enum_declaration" => {
                field_text(node, "name", src)
            }
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
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => rust_use_root(node, src),
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
        Language::C | Language::Cpp => {
            // `#include "geometry.h"` / `#include <math.h>` → the header path.
            let path = node.child_by_field_name("path")?;
            let raw = node_text(path, src).trim();
            Some(
                raw.trim_matches(|c| c == '"' || c == '<' || c == '>')
                    .to_string(),
            )
        }
        Language::Go => {
            // `import "math"` / `import m "math"` → the module path (unquoted).
            let path = node
                .child_by_field_name("path")
                .or_else(|| node.named_child(node.named_child_count().saturating_sub(1)))?;
            Some(node_text(path, src).trim_matches('"').to_string())
        }
        Language::Java => {
            // `import java.util.List;` → `java.util.List`.
            let mut c = node.walk();
            let found = node
                .named_children(&mut c)
                .find(|n| matches!(n.kind(), "scoped_identifier" | "identifier"));
            found.map(|n| node_text(n, src).to_string())
        }
    }
}

/// The root module of a Rust `use` declaration, mirroring CodeGraph 0.9.7's
/// `getRootModule`: the leftmost path segment (`crate`/`super`/`self`/a crate or
/// module name), ignoring any leading `pub`/visibility.
///
/// Returns `None` — so the import node is dropped — when the use argument is a
/// form CodeGraph also skips: a wildcard (`use x::*;`) or an aliased top-level
/// `use x as y;`. Only `scoped_use_list` / `scoped_identifier` / `use_list` /
/// `identifier` arguments produce an import (the exact set CodeGraph handles).
fn rust_use_root(node: Node, src: &str) -> Option<String> {
    // The use argument is the sole named child once any visibility_modifier /
    // attribute is skipped. Match CodeGraph's accepted argument kinds exactly.
    let mut c = node.walk();
    let arg = node.named_children(&mut c).find(|ch| {
        matches!(
            ch.kind(),
            "scoped_use_list" | "scoped_identifier" | "use_list" | "identifier"
        )
    })?;
    Some(rust_use_leftmost(arg, src))
}

/// The leftmost identifier of a Rust use-tree node (`crate::foo::Bar` → `crate`,
/// `geometry::{…}` → `geometry`).
fn rust_use_leftmost(node: Node, src: &str) -> String {
    let first = match node.named_child(0) {
        Some(f) => f,
        None => return node_text(node, src).to_string(),
    };
    match first.kind() {
        "identifier" | "crate" | "super" | "self" => node_text(first, src).to_string(),
        "scoped_identifier" | "scoped_use_list" => rust_use_leftmost(first, src),
        _ => node_text(first, src).to_string(),
    }
}

/// Whether an import definition is nested inside a function/closure body. Such
/// local imports are skipped to match CodeGraph 0.9.7, which records only
/// file/module-level (and, for Rust, `mod`-level) imports.
fn import_in_function_body(node: Node, lang: Language) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        let is_fn = match lang {
            Language::Rust => matches!(n.kind(), "function_item" | "closure_expression"),
            Language::Python => matches!(n.kind(), "function_definition" | "lambda"),
            Language::TypeScript | Language::Tsx | Language::JavaScript => matches!(
                n.kind(),
                "function_declaration"
                    | "function_expression"
                    | "arrow_function"
                    | "generator_function_declaration"
                    | "method_definition"
            ),
            // Tier-2 (C/C++/Go/Java) imports/includes are always file-level, so
            // none are ever function-body-nested; tier-3 has its own extractor.
            _ => false,
        };
        if is_fn {
            return true;
        }
        cur = n.parent();
    }
    false
}

// ---- signature -----------------------------------------------------------

/// The `signature` column, matching CodeGraph's convention per kind:
/// - callable (`function`/`method`) → the parameter list through the return
///   type, no `def`/`fn`/`async`/name/generics, internal newlines preserved;
/// - `import` → the full import statement text;
/// - `variable` → the assignment tail (`= float`);
/// - types/members (`class`/`struct`/`enum`/`type_alias`/`enum_member`) → NULL.
fn signature_for(node: Node, kind: &str, src: &str, lang: Language) -> Option<String> {
    if kind == "import" {
        let s = node_text(node, src).trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    match lang {
        // C/C++ record no signature for callables or types.
        Language::C | Language::Cpp => None,
        Language::Go => match kind {
            "function" | "method" => go_signature(node, src),
            "variable" | "constant" => go_assignment_signature(node, src),
            _ => None,
        },
        Language::Java => match kind {
            "method" => java_method_signature(node, src),
            "field" => java_field_signature(node, src),
            _ => None,
        },
        Language::Rust
        | Language::Python
        | Language::TypeScript
        | Language::Tsx
        | Language::JavaScript => match kind {
            "function" | "method" => callable_signature(node, src),
            "variable" => variable_signature(node, src),
            _ => None,
        },
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
    }
}

/// Go callable signature: the parameter list through the result type
/// (`(a, b Scalar) Scalar`), receiver excluded (it is a separate field).
fn go_signature(node: Node, src: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let end = node
        .child_by_field_name("result")
        .map(|r| r.end_byte())
        .unwrap_or_else(|| params.end_byte());
    let sig = src.get(params.start_byte()..end)?.trim();
    (!sig.is_empty()).then(|| sig.replace("\r\n", "\n"))
}

/// The `= value` assignment tail of a Go `var_spec`/`const_spec`, or `None` when
/// there is no initializer (`KindRectangle` in a `const (…iota…)` block).
fn go_assignment_signature(node: Node, src: &str) -> Option<String> {
    let val = node.child_by_field_name("value")?;
    let txt = node_text(val, src).trim();
    (!txt.is_empty()).then(|| format!("= {txt}"))
}

/// Java method/constructor signature: `<return-type> (<params>)`, or just
/// `(<params>)` for a constructor (no return type).
fn java_method_signature(node: Node, src: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let params_txt = node_text(params, src).trim();
    match node.child_by_field_name("type") {
        Some(ret) => Some(format!("{} {}", node_text(ret, src).trim(), params_txt)),
        None => Some(params_txt.to_string()),
    }
}

/// Java field signature: `<type> <name>` (`double UNIT`).
fn java_field_signature(node: Node, src: &str) -> Option<String> {
    let ty = node.child_by_field_name("type")?;
    let name = java_field_name(node, src)?;
    Some(format!("{} {}", node_text(ty, src).trim(), name))
}

/// `(params) -> ret` slice from the parameter list's `(` through the end of the
/// return type (or the params if there is none). Returns `None` when the node
/// has no parameter list (e.g. a TS class field), matching CodeGraph's NULL.
fn callable_signature(node: Node, src: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let end = node
        .child_by_field_name("return_type")
        .map(|r| r.end_byte())
        .unwrap_or_else(|| params.end_byte());
    let sig = src.get(params.start_byte()..end)?.trim();
    // Normalize CRLF → LF so a multi-line signature is deterministic regardless
    // of the checkout's line endings (and matches the golden fixture's `\n`).
    (!sig.is_empty()).then(|| sig.replace("\r\n", "\n"))
}

/// The assignment tail of a top-level variable (`Scalar = float` → `= float`).
fn variable_signature(node: Node, src: &str) -> Option<String> {
    let anchor = node
        .child_by_field_name("left")
        .or_else(|| node.child_by_field_name("name"))?;
    let sig = src.get(anchor.end_byte()..node.end_byte())?.trim();
    (!sig.is_empty()).then(|| sig.to_string())
}

// ---- docstring -----------------------------------------------------------

/// The `docstring` column.
///
/// Python uses the body's leading string literal (see [`python_docstring`]);
/// Rust and TS/JS use the definition's immediately-preceding doc-comment block.
///
/// This intentionally produces *cleaner and more complete* docstrings than
/// CodeGraph 0.9.7, which leaves Python docstrings NULL, keeps a stray leading
/// `/` on Rust `///` comments, can bleed a module `//!` header into the first
/// definition, and only captures a TS leading comment when it is the
/// definition's direct previous sibling (missing `export`-wrapped declarations).
/// We capture doc comments cleanly and consistently instead. The P2c parity
/// harness must whitelist this the same way it whitelists [`apply_flags`]'s
/// correct `is_async` (see #78).
fn docstring_for(node: Node, kind: &str, src: &str, lang: Language) -> Option<String> {
    if matches!(kind, "import" | "enum_member" | "file") {
        return None;
    }
    match lang {
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Python => python_docstring(node, src),
        Language::Rust
        | Language::TypeScript
        | Language::Tsx
        | Language::JavaScript
        | Language::C
        | Language::Cpp
        | Language::Go
        | Language::Java => leading_comment_docstring(node, src, lang),
    }
}

/// Collect the contiguous comment lines immediately above `node` (stopping at
/// the first blank line or code), strip their markers, and join them. This is
/// the clean, correct doc-comment rule — no blank-line skipping, no module-doc
/// bleed.
fn leading_comment_docstring(node: Node, src: &str, lang: Language) -> Option<String> {
    let start = node.start_byte();
    let before = &src[..start];
    // Lines strictly above the definition, nearest-first.
    let mut lines: Vec<&str> = before.split('\n').collect();
    // The last element is the (partial) line the definition starts on; drop it.
    lines.pop();

    let mut collected: Vec<String> = Vec::new();
    for raw in lines.iter().rev() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            break; // blank line ends the block
        }
        match strip_comment_marker(trimmed, lang) {
            Some(text) => collected.push(text),
            None => break, // hit code
        }
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    let joined = collected.join("\n").trim().to_string();
    (!joined.is_empty()).then_some(joined)
}

/// Strip a single doc/line-comment marker from `line`, returning the remaining
/// text, or `None` if the line is not a comment. `//!` inner-doc lines are
/// treated as file-level and excluded (return `None`) so they never attach to a
/// following item.
fn strip_comment_marker(line: &str, lang: Language) -> Option<String> {
    // Block-comment fragments (`/** ... */`, `* ...`, `*/`) — shared by all.
    if let Some(rest) = line.strip_prefix("/**") {
        return Some(rest.trim_end_matches("*/").trim().to_string());
    }
    if line == "*/" {
        return Some(String::new());
    }
    if let Some(rest) = line.strip_prefix('*') {
        // A `* text` JSDoc continuation (but not a `*/`-only line, handled above).
        return Some(rest.trim_end_matches("*/").trim().to_string());
    }
    match lang {
        // Rust and C/C++ use `///` doc lines (and Rust's `//!` inner doc, which
        // must not attach to a following item). We strip them cleanly — CodeGraph
        // 0.9.7 leaves a stray `/` on these (whitelisted as known-better in P2c).
        Language::Rust | Language::C | Language::Cpp => {
            if line.starts_with("//!") {
                return None; // inner/file doc — not this item's docstring
            }
            if let Some(rest) = line.strip_prefix("///") {
                return Some(rest.trim().to_string());
            }
            None
        }
        _ => line.strip_prefix("//").map(|rest| rest.trim().to_string()),
    }
}

// ---- flags ---------------------------------------------------------------

fn apply_flags(rec: &mut NodeRecord, node: Node, src: &str, lang: Language, kind: &str) {
    let header = node_text(node, src).lines().next().unwrap_or("");
    match lang {
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => {
            if matches!(kind, "function" | "method" | "struct" | "enum" | "trait")
                && has_child_kind(node, "visibility_modifier")
            {
                rec.visibility = Some("public".to_string());
            }
            // NOTE: we set is_async correctly for every language. CodeGraph
            // 0.9.7 only flags TS async functions (Rust/Python async defs are
            // recorded as is_async=0). This is a deliberate improvement; the
            // P2c parity harness must whitelist is_async as known-better.
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
        // C/C++ carry no visibility/flags in CodeGraph 0.9.7's output.
        Language::C | Language::Cpp => {}
        Language::Go => {
            // A Go identifier is "exported" when it starts uppercase — but 0.9.7
            // sets the flag only on type/func declarations, not var/const/method.
            if matches!(kind, "struct" | "interface" | "function" | "type_alias") {
                rec.is_exported = rec.name.chars().next().is_some_and(|c| c.is_uppercase());
            }
        }
        Language::Java => {
            if let Some(vis) = java_access_modifier(node) {
                rec.visibility = Some(vis.to_string());
            }
            rec.is_static = java_has_modifier(node, "static");
            rec.is_abstract = java_has_modifier(node, "abstract");
        }
    }
    if let Some(tp) = type_parameters(node, src) {
        rec.type_parameters = Some(tp);
    }
}

/// The access-level keyword (`public`/`private`/`protected`) in a Java
/// declaration's `modifiers`, if explicitly present.
fn java_access_modifier(node: Node) -> Option<&'static str> {
    ["public", "private", "protected"]
        .into_iter()
        .find(|&kw| java_has_modifier(node, kw))
}

/// Whether a Java declaration's leading `modifiers` node contains `keyword`.
fn java_has_modifier(node: Node, keyword: &str) -> bool {
    let mut c = node.walk();
    let Some(mods) = node
        .named_children(&mut c)
        .find(|n| n.kind() == "modifiers")
    else {
        return false;
    };
    let mut mc = mods.walk();
    let has = mods.children(&mut mc).any(|m| m.kind() == keyword);
    has
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
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
        Language::Python => tree_sitter::Language::new(tree_sitter_python::LANGUAGE),
        Language::TypeScript => {
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
        Language::Tsx | Language::JavaScript => {
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TSX)
        }
        Language::C => tree_sitter::Language::new(tree_sitter_c::LANGUAGE),
        Language::Cpp => tree_sitter::Language::new(tree_sitter_cpp::LANGUAGE),
        Language::Go => tree_sitter::Language::new(tree_sitter_go::LANGUAGE),
        Language::Java => tree_sitter::Language::new(tree_sitter_java::LANGUAGE),
    }
}

const RUST_QUERY: &str = include_str!("queries/rust.scm");
const PYTHON_QUERY: &str = include_str!("queries/python.scm");
const TYPESCRIPT_QUERY: &str = include_str!("queries/typescript.scm");
const C_QUERY: &str = include_str!("queries/c.scm");
const CPP_QUERY: &str = include_str!("queries/cpp.scm");
const GO_QUERY: &str = include_str!("queries/go.scm");
const JAVA_QUERY: &str = include_str!("queries/java.scm");

const RUST_REFS_QUERY: &str = include_str!("queries/rust.refs.scm");
const PYTHON_REFS_QUERY: &str = include_str!("queries/python.refs.scm");
const TYPESCRIPT_REFS_QUERY: &str = include_str!("queries/typescript.refs.scm");
const C_REFS_QUERY: &str = include_str!("queries/c.refs.scm");
const CPP_REFS_QUERY: &str = include_str!("queries/cpp.refs.scm");
const GO_REFS_QUERY: &str = include_str!("queries/go.refs.scm");
const JAVA_REFS_QUERY: &str = include_str!("queries/java.refs.scm");

fn query_source(lang: Language) -> &'static str {
    match lang {
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => RUST_QUERY,
        Language::Python => PYTHON_QUERY,
        Language::TypeScript | Language::Tsx | Language::JavaScript => TYPESCRIPT_QUERY,
        Language::C => C_QUERY,
        Language::Cpp => CPP_QUERY,
        Language::Go => GO_QUERY,
        Language::Java => JAVA_QUERY,
    }
}

fn refs_query_source(lang: Language) -> &'static str {
    match lang {
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => RUST_REFS_QUERY,
        Language::Python => PYTHON_REFS_QUERY,
        Language::TypeScript | Language::Tsx | Language::JavaScript => TYPESCRIPT_REFS_QUERY,
        Language::C => C_REFS_QUERY,
        Language::Cpp => CPP_REFS_QUERY,
        Language::Go => GO_REFS_QUERY,
        Language::Java => JAVA_REFS_QUERY,
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
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => cache!(RUST),
        Language::Python => cache!(PYTHON),
        Language::TypeScript => cache!(TS),
        Language::Tsx => cache!(TSX),
        Language::JavaScript => cache!(JS),
        Language::C => cache!(C),
        Language::Cpp => cache!(CPP),
        Language::Go => cache!(GO),
        Language::Java => cache!(JAVA),
    }
}

/// Compile (and cache) the grammar + **reference-site** query for a language.
/// Same caching contract as [`compiled`]: the pair is `Sync` and shared across
/// rayon workers.
fn compiled_refs(lang: Language) -> &'static (tree_sitter::Language, Query) {
    macro_rules! cache {
        ($cell:ident) => {{
            static $cell: OnceLock<(tree_sitter::Language, Query)> = OnceLock::new();
            $cell.get_or_init(|| {
                let l = ts_language(lang);
                let q = Query::new(&l, refs_query_source(lang)).expect("refs query compiles");
                (l, q)
            })
        }};
    }
    match lang {
        Language::Ruby
        | Language::Php
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::CSharp => unreachable!("tier-3 handled by tier3::extract"),
        Language::Rust => cache!(RUST_REFS),
        Language::Python => cache!(PYTHON_REFS),
        Language::TypeScript => cache!(TS_REFS),
        Language::Tsx => cache!(TSX_REFS),
        Language::JavaScript => cache!(JS_REFS),
        Language::C => cache!(C_REFS),
        Language::Cpp => cache!(CPP_REFS),
        Language::Go => cache!(GO_REFS),
        Language::Java => cache!(JAVA_REFS),
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
        // 3 content lines + a trailing newline. CodeGraph counts lines as
        // `split('\n').length`, so the phantom trailing segment makes end_line 4.
        let ex = extract("line1\nline2\nline3\n", "empty.py", Language::Python, 0, 0);
        let file = ex.nodes.iter().find(|n| n.kind == "file").unwrap();
        assert_eq!(file.name, "empty.py");
        assert_eq!(file.qualified_name, "empty.py");
        assert_eq!(file.start_line, 1);
        assert_eq!(file.end_line, 4);
        // No trailing newline → no phantom line.
        let ex2 = extract("a\nb", "x.py", Language::Python, 0, 0);
        let f2 = ex2.nodes.iter().find(|n| n.kind == "file").unwrap();
        assert_eq!(f2.end_line, 2);
        // node_count on the files row includes the file node itself.
        assert_eq!(ex.file_record.node_count, ex.nodes.len() as u64);
    }

    #[test]
    fn signatures_match_codegraph_convention() {
        // Rust: params through return type, no `fn name`.
        let rs = extract_nodes("pub fn f(a: i32) -> i32 { a }\n", "a.rs", Language::Rust);
        assert_eq!(
            find(&rs, "f").unwrap().signature.as_deref(),
            Some("(a: i32) -> i32")
        );
        // Type has NULL signature.
        let st = extract_nodes("pub struct S { x: i32 }\n", "a.rs", Language::Rust);
        assert_eq!(find(&st, "S").unwrap().signature, None);
        // Python: return annotation kept, no `def name`.
        let py = extract_nodes(
            "def g(a: int) -> int:\n    return a\n",
            "a.py",
            Language::Python,
        );
        assert_eq!(
            find(&py, "g").unwrap().signature.as_deref(),
            Some("(a: int) -> int")
        );
        // Variable: assignment tail.
        let v = extract_nodes("Scalar = float\n", "a.py", Language::Python);
        assert_eq!(
            find(&v, "Scalar").unwrap().signature.as_deref(),
            Some("= float")
        );
        // Import: full statement.
        let im = extract_nodes("import asyncio\n", "a.py", Language::Python);
        assert_eq!(
            find(&im, "asyncio").unwrap().signature.as_deref(),
            Some("import asyncio")
        );
    }

    #[test]
    fn rust_and_ts_doc_comments_are_captured_cleanly() {
        // Rust `///` — no stray leading slash, no module-`//!` bleed.
        let rs = extract_nodes(
            "//! module doc\n\n/// The answer.\npub fn answer() -> i32 { 42 }\n",
            "a.rs",
            Language::Rust,
        );
        assert_eq!(
            find(&rs, "answer").unwrap().docstring.as_deref(),
            Some("The answer.")
        );
        // TS `//` line comment directly above a method.
        let ts = extract_nodes(
            "class C {\n  // does a thing\n  m(): void {}\n}\n",
            "a.ts",
            Language::TypeScript,
        );
        assert_eq!(
            find(&ts, "C::m").unwrap().docstring.as_deref(),
            Some("does a thing")
        );
        // A blank line between comment and def breaks the block.
        let rs2 = extract_nodes("/// far away\n\npub fn g() {}\n", "a.rs", Language::Rust);
        assert_eq!(find(&rs2, "g").unwrap().docstring, None);
    }

    // ---- tier-2 (C / C++ / Go / Java) ------------------------------------

    #[test]
    fn c_defs_typedef_struct_enum_no_globals() {
        let src = "/** doc. */\ntypedef double Scalar;\nstruct P { double x; };\n\
                   enum E { A, B };\nconst double UNIT = 1.0;\ndouble f(double a) { return a; }\n";
        let nodes = extract_nodes(src, "a.c", Language::C);
        assert_eq!(find(&nodes, "Scalar").unwrap().kind, "type_alias");
        assert_eq!(find(&nodes, "P").unwrap().kind, "struct");
        assert_eq!(find(&nodes, "E::A").unwrap().kind, "enum_member");
        // C: no top-level variable node, no struct-field nodes, function sig NULL.
        assert!(find(&nodes, "UNIT").is_none());
        assert!(find(&nodes, "P::x").is_none());
        assert_eq!(find(&nodes, "f").unwrap().signature, None);
    }

    #[test]
    fn cpp_out_of_line_method_and_clean_doc() {
        let src = "using Scalar = double;\nclass P { public: Scalar m() const; };\n\
                   /// does it\nScalar P::m() const { return 0; }\n";
        let nodes = extract_nodes(src, "a.cpp", Language::Cpp);
        // The in-class declaration is not a node; the out-of-line definition is.
        let m = find(&nodes, "P::m").unwrap();
        assert_eq!(m.kind, "method");
        assert_eq!(m.signature, None);
        // `///` doc captured cleanly (no stray leading `/`).
        assert_eq!(m.docstring.as_deref(), Some("does it"));
        assert_eq!(find(&nodes, "Scalar").unwrap().kind, "type_alias");
    }

    #[test]
    fn go_kinds_signatures_and_export_flag() {
        let src = "package p\n\ntype Scalar = float64\ntype Kind int\n\
                   type Point struct { X Scalar }\ntype Shape interface { Area() Scalar }\n\
                   var Unit Scalar = 1.0\n\nfunc Hypot(a, b Scalar) Scalar { return a }\n\
                   func (p Point) Dist(o Point) Scalar { return Hypot(p.X, o.X) }\n";
        let nodes = extract_nodes(src, "a.go", Language::Go);
        // `type X = Y` alias emits no node; `type X int` does (as type_alias).
        assert!(find(&nodes, "Scalar").is_none());
        assert_eq!(find(&nodes, "Kind").unwrap().kind, "type_alias");
        assert_eq!(find(&nodes, "Point").unwrap().kind, "struct");
        assert_eq!(find(&nodes, "Shape").unwrap().kind, "interface");
        // Method qualified by its receiver; signature excludes the receiver.
        let dist = find(&nodes, "Point::Dist").unwrap();
        assert_eq!(dist.kind, "method");
        assert_eq!(dist.signature.as_deref(), Some("(o Point) Scalar"));
        assert_eq!(
            find(&nodes, "Hypot").unwrap().signature.as_deref(),
            Some("(a, b Scalar) Scalar")
        );
        // is_exported on type/func decls only.
        assert!(find(&nodes, "Point").unwrap().is_exported);
        assert!(find(&nodes, "Hypot").unwrap().is_exported);
        assert!(!find(&nodes, "Unit").unwrap().is_exported);
        assert!(!dist.is_exported);
    }

    #[test]
    fn java_namespace_qn_field_and_method_signatures() {
        let src = "package fixture;\n\npublic class C {\n  public static final double U = 1.0;\n\
                   public double m(double a) { return a; }\n  public C(double a) {}\n}\n";
        let nodes = extract_nodes(src, "C.java", Language::Java);
        assert_eq!(find(&nodes, "fixture").unwrap().kind, "namespace");
        let c = find(&nodes, "fixture::C").unwrap();
        assert_eq!(c.kind, "class");
        assert_eq!(c.visibility.as_deref(), Some("public"));
        let u = find(&nodes, "fixture::C::U").unwrap();
        assert_eq!(u.kind, "field");
        assert_eq!(u.signature.as_deref(), Some("double U"));
        assert!(u.is_static);
        // Method: `<ret> (<params>)`; constructor: `(<params>)` (no return).
        assert_eq!(
            find(&nodes, "fixture::C::m").unwrap().signature.as_deref(),
            Some("double (double a)")
        );
        assert_eq!(
            find(&nodes, "fixture::C::C").unwrap().signature.as_deref(),
            Some("(double a)")
        );
    }

    // ---- tier-2 inheritance (edge alignment, issue #78) ---------------------

    #[test]
    fn cpp_base_class_clause_is_extends_no_spurious() {
        // Single + multiple inheritance → `extends` for every base (C++ has no
        // `implements`). `double` returns avoid CodeGraph's return-type-misread,
        // so we emit exactly the genuine bases.
        let src = "\
class Solid { public: double d; };
class Drawable { public: virtual double area() const = 0; };
class Disc : public Solid { public: double r; };
class Prism : public Solid, public Drawable { public: double area() const; };
";
        let ex = extract(src, "a.cpp", Language::Cpp, 0, 0);
        let edges = inheritance_edges(&ex);
        assert!(edges.contains(&("extends".into(), "Disc".into(), "Solid".into())));
        assert!(edges.contains(&("extends".into(), "Prism".into(), "Solid".into())));
        assert!(edges.contains(&("extends".into(), "Prism".into(), "Drawable".into())));
        assert_eq!(
            edges.len(),
            3,
            "no implements, no spurious extends: {edges:?}"
        );
    }

    #[test]
    fn go_struct_and_interface_embedding_is_extends() {
        let src = "package p
type Base struct { X int }
type Disc struct { Base; R int }
type Reader interface { Read() int }
type RW interface { Reader; Write(v int) int }
";
        let ex = extract(src, "a.go", Language::Go, 0, 0);
        let edges = inheritance_edges(&ex);
        // Embedded (unnamed) fields/types → extends; the named field `R` does not.
        assert!(edges.contains(&("extends".into(), "Disc".into(), "Base".into())));
        assert!(edges.contains(&("extends".into(), "RW".into(), "Reader".into())));
        assert_eq!(edges.len(), 2, "only embeds, not named fields: {edges:?}");
    }

    #[test]
    fn java_class_extends_implements_and_interface_extends() {
        let src = "package p;
interface Shape { double area(); }
interface Named extends Shape { String name(); }
abstract class Base { double t; }
class Circle extends Base implements Shape, Named {
    public double area() { return 0.0; }
    public String name() { return \"c\"; }
}
";
        let ex = extract(src, "A.java", Language::Java, 0, 0);
        let edges = inheritance_edges(&ex);
        assert!(edges.contains(&("extends".into(), "p::Circle".into(), "p::Base".into())));
        assert!(edges.contains(&("implements".into(), "p::Circle".into(), "p::Shape".into())));
        assert!(edges.contains(&("implements".into(), "p::Circle".into(), "p::Named".into())));
        // A Java interface's `extends` targets an interface (unlike TS, which
        // CodeGraph misses) and is recorded as `extends`.
        assert!(edges.contains(&("extends".into(), "p::Named".into(), "p::Shape".into())));
    }

    // ---- tier-1 hardening: trait/interface nodes + inheritance edges --------

    /// Collect `(kind, source_qn, target_qn)` for the extends/implements edges an
    /// extract resolves against its own nodes (single-file resolution).
    fn inheritance_edges(ex: &FileExtract) -> Vec<(String, String, String)> {
        use crate::resolve::SymbolTable;
        let edges = SymbolTable::build(&ex.nodes).resolve_all(&ex.sites);
        let by_id = |id: &str| {
            ex.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.qualified_name.clone())
                .unwrap_or_default()
        };
        edges
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "extends" | "implements"))
            .map(|e| (e.kind.clone(), by_id(&e.source), by_id(&e.target)))
            .collect()
    }

    #[test]
    fn rust_trait_nodes_supertrait_extends_and_impl_implements() {
        let src = "\
pub trait Shape { fn area(&self) -> f64; }
pub trait Named: Shape { fn label(&self) -> String; }
pub struct Circle;
impl Shape for Circle { fn area(&self) -> f64 { 0.0 } }
impl Circle { fn helper(&self) {} }
";
        let ex = extract(src, "a.rs", Language::Rust, 0, 0);
        // trait nodes exist with kind `trait`.
        assert_eq!(find(&ex.nodes, "Shape").unwrap().kind, "trait");
        assert_eq!(find(&ex.nodes, "Named").unwrap().kind, "trait");
        let edges = inheritance_edges(&ex);
        // Supertrait: Named extends Shape.
        assert!(edges.contains(&("extends".into(), "Named".into(), "Shape".into())));
        // impl Shape for Circle → Circle implements Shape.
        assert!(edges.contains(&("implements".into(), "Circle".into(), "Shape".into())));
        // Plain `impl Circle {}` emits no inheritance edge.
        assert_eq!(
            edges.iter().filter(|(k, _, _)| k == "implements").count(),
            1
        );
    }

    #[test]
    fn ts_interface_nodes_and_class_extends_implements() {
        let src = "\
export interface Shape { area(): number; }
export class Base {}
export class Circle extends Base implements Shape {
  area(): number { return 0; }
}
";
        let ex = extract(src, "a.ts", Language::TypeScript, 0, 0);
        assert_eq!(find(&ex.nodes, "Shape").unwrap().kind, "interface");
        let edges = inheritance_edges(&ex);
        assert!(edges.contains(&("extends".into(), "Circle".into(), "Base".into())));
        assert!(edges.contains(&("implements".into(), "Circle".into(), "Shape".into())));
    }

    #[test]
    fn python_class_inheritance_is_extends() {
        let src = "\
class Base:
    pass

class Derived(Base):
    pass
";
        let ex = extract(src, "a.py", Language::Python, 0, 0);
        let edges = inheritance_edges(&ex);
        assert!(edges.contains(&("extends".into(), "Derived".into(), "Base".into())));
    }

    #[test]
    fn rust_imports_are_unqualified_deduped_by_line_and_scope_filtered() {
        let src = "\
use crate::foo::Bar;
use crate::baz::Qux;
pub use crate::model::Thing;
use std::collections::HashMap;

pub struct S;
impl S {
    pub fn go(&self) {
        use std::fmt::Write;
        let _ = HashMap::<u8, u8>::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
}
";
        let nodes = extract_nodes(src, "a.rs", Language::Rust);
        let imports: Vec<&NodeRecord> = nodes.iter().filter(|n| n.kind == "import").collect();
        // Every import is named by its root and NOT qualified by an enclosing
        // scope (no `tests::std`, no `pub`).
        assert!(imports.iter().all(|i| !i.qualified_name.contains("::")));
        assert!(imports.iter().all(|i| i.name != "pub"));
        // `pub use crate::…` roots at `crate`, not `pub`.
        assert_eq!(
            imports.iter().filter(|i| i.name == "crate").count(),
            3,
            "three `use crate::…` (incl the pub use), each a distinct node"
        );
        // The `use std::fmt::Write` inside `fn go` is skipped (function-body).
        // Module-level `use std::…` (HashMap) and the `mod tests` `use std::sync`
        // are kept → two distinct `std` imports.
        assert_eq!(
            imports.iter().filter(|i| i.name == "std").count(),
            2,
            "module-level + mod-level std imports kept; fn-body one skipped"
        );
        // Wildcard `use super::*` is skipped (CodeGraph skips use_wildcard).
        assert!(imports.iter().all(|i| i.name != "super"));
    }

    #[test]
    fn self_field_method_call_is_typed_from_struct_fields() {
        // `self.db.query()` should carry recv_type = the field's type (Db), so the
        // resolver can resolve `Db::query`.
        let src = "\
pub struct Db;
impl Db { pub fn query(&self) {} }
pub struct App { db: Db }
impl App {
    pub fn run(&self) { self.db.query(); }
}
";
        let ex = extract(src, "a.rs", Language::Rust, 0, 0);
        use crate::resolve::SymbolTable;
        let edges = SymbolTable::build(&ex.nodes).resolve_all(&ex.sites);
        let target_qn = |id: &str| {
            ex.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.qualified_name.as_str())
                .unwrap_or("")
        };
        assert!(
            edges
                .iter()
                .any(|e| e.kind == "calls" && target_qn(&e.target) == "Db::query"),
            "self.db.query() resolves to Db::query via field typing"
        );
    }

    /// End-to-end (parse → resolve) receiver-type inference: return-type-of-local,
    /// chained call, and for-loop element — all evidence-based, and an
    /// un-inferrable receiver (`x` from an unknown source) still drops.
    #[test]
    fn receiver_inference_end_to_end() {
        use crate::resolve::SymbolTable;
        let src = "\
pub struct Db;
impl Db {
    pub fn open() -> Db { Db }
    pub fn query(&self) {}
    pub fn rows(&self) -> Vec<Row> { Vec::new() }
}
pub struct Row;
impl Row { pub fn cell(&self) {} }
pub fn all() -> Vec<Db> { Vec::new() }
pub fn go(x: Unknown) {
    let db = Db::open();
    db.query();                 // return-type-of-local -> Db::query
    Db::open().query();         // chained -> Db::query
    for d in all() { d.query(); } // for-loop element -> Db::query
    for r in db.rows() { r.cell(); } // chained-call for-loop -> Row::cell
    x.mystery();                // un-inferrable receiver -> dropped
}
";
        let ex = extract(src, "a.rs", Language::Rust, 0, 0);
        let edges = SymbolTable::build(&ex.nodes).resolve_all(&ex.sites);
        let tqn = |id: &str| {
            ex.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.qualified_name.clone())
                .unwrap_or_default()
        };
        let calls: Vec<String> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| tqn(&e.target))
            .collect();
        // Db::query resolved from a local, a chain, and a for-loop element.
        assert!(
            calls.iter().filter(|q| *q == "Db::query").count() >= 3,
            "expected >=3 Db::query calls, got {calls:?}"
        );
        // The chained-call for-loop `for r in db.rows()` types `r` as Row.
        assert!(calls.iter().any(|q| q == "Row::cell"), "{calls:?}");
        // The un-inferrable `x.mystery()` is dropped (no `mystery` target).
        assert!(
            !calls
                .iter()
                .any(|q| q.ends_with("::mystery") || q == "mystery"),
            "un-inferrable receiver must drop: {calls:?}"
        );
    }

    /// Typed-Python: a parameter annotation and a constructor assignment type the
    /// receiver.
    #[test]
    fn python_typed_receiver_inference() {
        use crate::resolve::SymbolTable;
        let src = "\
class Agent:
    def ask(self): ...

class Client:
    def send(self): ...

def run(c: Client):
    c.send()          # param annotation -> Client::send
    agent = Agent()
    agent.ask()       # constructor assignment -> Agent::ask
";
        let ex = extract(src, "a.py", Language::Python, 0, 0);
        let edges = SymbolTable::build(&ex.nodes).resolve_all(&ex.sites);
        let tqn = |id: &str| {
            ex.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.qualified_name.clone())
                .unwrap_or_default()
        };
        let calls: Vec<String> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| tqn(&e.target))
            .collect();
        assert!(calls.iter().any(|q| q == "Client::send"), "{calls:?}");
        assert!(calls.iter().any(|q| q == "Agent::ask"), "{calls:?}");
    }

    /// rev-44 BLOCKER (end-to-end): a TS/JS `T[]`-returning call must not
    /// fabricate an element-type method call. `const list = items(); list.find()`
    /// is `Array.prototype.find`, not `Query::find`.
    #[test]
    fn ts_array_return_does_not_fabricate_method_call() {
        use crate::resolve::SymbolTable;
        let src = "\
class Query { run(): void {} }
function items(): Query[] { return []; }
function go(): void {
  const list = items();
  list.run();
}
";
        let ex = extract(src, "a.ts", Language::TypeScript, 0, 0);
        let edges = SymbolTable::build(&ex.nodes).resolve_all(&ex.sites);
        let tqn = |id: &str| {
            ex.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.qualified_name.clone())
                .unwrap_or_default()
        };
        assert!(
            !edges
                .iter()
                .any(|e| e.kind == "calls" && tqn(&e.target) == "Query::run"),
            "list.run() on a Query[] must not resolve to Query::run"
        );
    }
}
