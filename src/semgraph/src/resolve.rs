//! Reference-resolution layer (issue #78, Phase 2b).
//!
//! Pass 1 (in [`crate::parse`]) extracts, per file, the definition nodes plus a
//! list of **reference sites** — call/method-call/constructor/type/import
//! occurrences with their call-site coordinates and whatever local context the
//! parser could infer (a qualified path, or a receiver's inferred type). Pass 2,
//! here, builds a global symbol table over *all* definitions and resolves each
//! site to a target node, emitting `calls` / `references` / `imports` /
//! `instantiates` [`EdgeRecord`]s.
//!
//! ## Resolution is deterministic
//!
//! A site resolves purely as a function of the global symbol table (name /
//! qualified-name lookups) and the site's own file — never of file or thread
//! order. The same source tree therefore yields the same edge set regardless of
//! how the parallel parse pass happened to schedule. Ambiguity is broken by a
//! fixed precedence, then by lexicographic `file_path`, so ties are resolved
//! identically every run.
//!
//! ## Precedence (matching CodeGraph 0.9.7's name-based heuristics)
//!
//! For a bare name the candidate is chosen by, in order:
//! same-file → import-target → unique global → same-directory → same-language.
//! A name that stays ambiguous after all of these is **dropped** — precision
//! over recall for `calls` edges, as the issue directs. Qualified `A::b` calls
//! resolve against `qualified_name` directly (and may target an `enum_member`, so
//! a tuple-variant constructor `Error::Sqlite(e)` becomes a `calls` edge). Method
//! calls (`recv.m()`) resolve **only** from the receiver's inferred type — a
//! local `let`/parameter, `self`, or a `self.field` typed from the enclosing
//! type's declared fields → `Type::m`. An un-inferrable receiver is **dropped**,
//! never resolved by method name alone: a bare name collides with std/library
//! methods of the same name (`.as_str()`, `.get()`), so name resolution would
//! fabricate — precision over recall, as the issue directs.
//!
//! ## `unresolved_refs` stays empty (documented deviation-that-isn't)
//!
//! CodeGraph 0.9.7 drains `unresolved_refs` after every batch, so a finished DB
//! always has 0 rows (see `tests/fixtures/README.md` and ADR-003). We match that
//! observable behavior: a site we cannot resolve is simply dropped, never
//! persisted. The reader already treats 0 rows as normal and the P2c parity
//! harness sees the same empty table CodeGraph produces.

use std::collections::{HashMap, HashSet};

use crate::model::{EdgeRecord, NodeRecord};

/// A reference site discovered during pass-1 parsing, awaiting resolution.
#[derive(Debug, Clone)]
pub(crate) struct RawSite {
    /// Node id of the enclosing definition — the *source* of the emitted edge.
    /// For an import site this is the file node's id.
    pub from_id: String,
    /// 1-based line of the call/reference site (call-expression start, type
    /// identifier, or import statement).
    pub line: u32,
    /// 0-based column of the site.
    pub col: u32,
    pub payload: SitePayload,
}

/// The kind of site and the local context the parser resolved for it.
#[derive(Debug, Clone)]
pub(crate) enum SitePayload {
    /// A bare `name(...)` call: resolves to a `function` (→ `calls`) or, failing
    /// that, a class-like node (→ `instantiates`, e.g. Python `Circle(r)`).
    CallOrCtor { name: String },
    /// A qualified `qualifier::name(...)` path call (Rust) → `calls`.
    QualifiedCall { qualifier: String, name: String },
    /// A `recv.name(...)` method call. `recv_type` is the receiver's locally
    /// inferred type, or `None` when it could not be inferred (then dropped).
    MethodCall {
        recv_type: Option<String>,
        name: String,
    },
    /// A `new Name(...)` expression (TS/JS) → `instantiates`.
    New { name: String },
    /// A type identifier appearing in a signature/field annotation → `references`.
    /// Resolves against any type-like kind, including `type_alias`.
    TypeRef { name: String },
    /// Like [`SitePayload::TypeRef`] but resolving *only* to a concrete type
    /// (`struct`/`interface`/`class`/`enum`, never a `type_alias`) — Go and Java,
    /// where CodeGraph never references a plain alias. See `crate::parse`.
    TypeRefStrict { name: String },
    /// An import statement: a file → import-node edge. `module` is the imported
    /// module string (used only for metadata / module→file resolution).
    Import { target_id: String, module: String },
    /// An inheritance relationship — a Rust supertrait / Python base class / TS
    /// `extends` (→ `extends`), or a Rust `impl Trait for Type` / TS
    /// `implements` (→ `implements`). `parent` is the referenced type name; the
    /// edge source is the inheriting definition's node id (`from_id`).
    Inherit {
        parent: String,
        edge_kind: &'static str,
    },
    /// An inheritance relationship whose kind (`extends` vs `implements`) is
    /// decided by the *resolved target's* kind, not by syntax — used by Swift and
    /// C#, where a single `: A, B, C` base list has no syntactic marker
    /// distinguishing a superclass from a conformed interface. Matches CodeGraph
    /// 0.9.7: a target `interface` (Swift protocol / C# interface) → `implements`,
    /// anything else (a class/struct base) → `extends`. `parent` is the type name.
    InheritAuto { parent: String },
}

/// A resolved target plus how it was found (for the edge's `metadata`).
struct Resolved<'a> {
    node: &'a NodeRecord,
    resolved_by: &'static str,
    confidence: f64,
}

/// Node kinds that a bare/qualified *call* may target.
const CALLABLE_KINDS: &[&str] = &["function", "method"];
/// Node kinds a *qualified* call (`Type::x(...)`) may target. Adds `enum_member`
/// so a tuple-variant constructor call (`LanceError::MissingFile(p)`,
/// `Error::Sqlite(e)`) resolves to the variant as a `calls` edge, matching
/// CodeGraph 0.9.7.
const QUALIFIED_CALL_KINDS: &[&str] = &["function", "method", "enum_member"];
/// Node kinds a constructor/`new` may target.
const CLASS_KINDS: &[&str] = &["class", "struct", "interface"];
/// Node kinds a type reference may target.
const TYPE_KINDS: &[&str] = &["type_alias", "struct", "enum", "class", "interface"];
/// Node kinds a *strict* type reference may target (no `type_alias`).
const STRICT_TYPE_KINDS: &[&str] = &["struct", "enum", "class", "interface"];
/// Node kinds an `extends` edge may target (a base class / supertrait / base
/// interface).
const EXTENDS_KINDS: &[&str] = &["class", "struct", "interface", "trait", "enum"];
/// Node kinds an `implements` edge may target (a trait / interface).
const IMPLEMENTS_KINDS: &[&str] = &["trait", "interface", "class"];
/// Node kinds an [`SitePayload::InheritAuto`] site may target — the union of the
/// extends/implements target kinds. The resolved node's own kind then decides
/// whether the edge is `extends` or `implements`.
const INHERIT_ANY_KINDS: &[&str] = &["class", "struct", "interface", "trait", "enum"];

/// A global index over every definition node, used to resolve reference sites.
///
/// Borrows the node slice; lookups return indices into it. Built once per index
/// or sync pass after all files are parsed.
pub(crate) struct SymbolTable<'a> {
    nodes: &'a [NodeRecord],
    /// Non-file, non-import node indices keyed by plain `name`.
    by_name: HashMap<&'a str, Vec<usize>>,
    /// Non-file, non-import node indices keyed by `qualified_name`.
    by_qn: HashMap<&'a str, Vec<usize>>,
    /// For each file path, the set of *other* file paths it imports (resolved
    /// from its import statements) — the "import-target" precedence tier.
    imported_files: HashMap<&'a str, Vec<String>>,
    /// Node id → its `file_path`, so resolving a site's `from_id` is O(1) rather
    /// than an O(nodes) scan per site.
    id_to_file: HashMap<&'a str, &'a str>,
    /// File path → its `language`, so language scoping is O(1) per lookup.
    file_to_lang: HashMap<&'a str, &'a str>,
    /// The set of file paths present in the graph (for include→file resolution).
    file_paths: HashSet<&'a str>,
}

impl<'a> SymbolTable<'a> {
    /// Build the table over `nodes`. Import→file edges are resolved up front
    /// from the `import` definition nodes so the resolver can prefer a symbol
    /// that comes from a file the caller actually imports.
    pub fn build(nodes: &'a [NodeRecord]) -> SymbolTable<'a> {
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_qn: HashMap<&str, Vec<usize>> = HashMap::new();
        let file_paths: HashSet<&str> = nodes
            .iter()
            .filter(|n| n.kind == "file")
            .map(|n| n.file_path.as_str())
            .collect();

        let mut id_to_file: HashMap<&str, &str> = HashMap::with_capacity(nodes.len());
        let mut file_to_lang: HashMap<&str, &str> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            id_to_file.insert(n.id.as_str(), n.file_path.as_str());
            file_to_lang
                .entry(n.file_path.as_str())
                .or_insert(n.language.as_str());
            if n.kind == "file" || n.kind == "import" {
                continue;
            }
            by_name.entry(n.name.as_str()).or_default().push(i);
            by_qn.entry(n.qualified_name.as_str()).or_default().push(i);
        }

        // Resolve each import statement's module string to a concrete file.
        let mut imported_files: HashMap<&str, Vec<String>> = HashMap::new();
        for n in nodes.iter().filter(|n| n.kind == "import") {
            if let Some(target) = resolve_module_to_file(&n.file_path, &n.name, &file_paths) {
                imported_files
                    .entry(n.file_path.as_str())
                    .or_default()
                    .push(target);
            }
        }

        SymbolTable {
            nodes,
            by_name,
            by_qn,
            imported_files,
            id_to_file,
            file_to_lang,
            file_paths,
        }
    }

    /// Resolve every site in `sites` (from every file) to zero or one edge.
    ///
    /// Returns edges sorted deterministically by `(source, kind, line, col,
    /// target)` so the written `edges` table is stable across runs regardless of
    /// parse order. Sites that do not resolve are dropped (see the module docs on
    /// `unresolved_refs`).
    pub fn resolve_all(&self, sites: &[RawSite]) -> Vec<EdgeRecord> {
        let mut edges = Vec::new();
        let mut seen: HashSet<(String, String, String, u32, u32)> = HashSet::new();
        for site in sites {
            if let Some(edge) = self.resolve_site(site) {
                // Dedup exact (source,target,kind,line,col) repeats — a defensive
                // guard; distinct call sites keep distinct coordinates.
                let key = (
                    edge.source.clone(),
                    edge.target.clone(),
                    edge.kind.clone(),
                    edge.line.unwrap_or(0),
                    edge.col.unwrap_or(0),
                );
                if seen.insert(key) {
                    edges.push(edge);
                }
            }
        }
        edges.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.col.cmp(&b.col))
                .then_with(|| a.target.cmp(&b.target))
        });
        edges
    }

    fn resolve_site(&self, site: &RawSite) -> Option<EdgeRecord> {
        let from_file = self.file_of(&site.from_id)?;
        match &site.payload {
            SitePayload::CallOrCtor { name } => {
                // A function call takes precedence; a class match becomes an
                // instantiation (Python `Circle(r)`), matching CodeGraph.
                if let Some(r) = self.resolve_name(from_file, name, CALLABLE_KINDS) {
                    Some(self.edge(site, "calls", &r))
                } else {
                    let r = self.resolve_name(from_file, name, CLASS_KINDS)?;
                    Some(self.edge(site, "instantiates", &r))
                }
            }
            SitePayload::QualifiedCall { qualifier, name } => {
                let qn = format!("{qualifier}::{name}");
                let r = self.resolve_qualified(from_file, &qn, QUALIFIED_CALL_KINDS)?;
                Some(self.edge_meta(site, "calls", r.node, "qualified-name", 0.95))
            }
            SitePayload::MethodCall { recv_type, name } => {
                // Resolve ONLY when the receiver's type was inferred locally (a
                // typed local/parameter, `self`, or a `self.field` typed from the
                // enclosing type's fields) → `Type::name`. An un-inferrable
                // receiver is DROPPED, never resolved by method name alone: a bare
                // method name routinely collides with a std/library method of the
                // same name (`.as_str()` on a `String`, `.get()` on a `HashMap`),
                // so name-based resolution would *fabricate* an edge to a
                // same-named project symbol. Precision over recall (issue #78);
                // CodeGraph's name-resolved edges that we decline are whitelisted
                // as fabrications, not imitated.
                let ty = recv_type.as_ref()?;
                let qn = format!("{ty}::{name}");
                let r = self.resolve_qualified(from_file, &qn, CALLABLE_KINDS)?;
                Some(self.edge_meta(site, "calls", r.node, "instance-method", 0.7))
            }
            SitePayload::New { name } => {
                let r = self.resolve_name(from_file, name, CLASS_KINDS)?;
                Some(self.edge(site, "instantiates", &r))
            }
            SitePayload::TypeRef { name } => {
                let r = self.resolve_name(from_file, name, TYPE_KINDS)?;
                Some(self.edge(site, "references", &r))
            }
            SitePayload::TypeRefStrict { name } => {
                let r = self.resolve_name(from_file, name, STRICT_TYPE_KINDS)?;
                Some(self.edge(site, "references", &r))
            }
            SitePayload::Import { target_id, module } => {
                // C/C++ `#include "local.h"` that resolves to a file in the graph
                // targets that file's node (CodeGraph does the same); an
                // unresolved/system include falls back to the import node.
                if matches!(self.language_of(from_file), Some("c") | Some("cpp")) {
                    if let Some(path) = resolve_include_to_file(from_file, module, &self.file_paths)
                    {
                        return Some(EdgeRecord {
                            source: site.from_id.clone(),
                            target: format!("file:{path}"),
                            kind: "imports".to_string(),
                            metadata: Some(metadata_json(0.9, "import")),
                            line: Some(site.line),
                            col: Some(site.col),
                            provenance: None,
                        });
                    }
                }
                // Import edges always land (the target is our own import node);
                // a resolvable relative module reads as a stronger match.
                let (by, conf) = if module.starts_with('.') {
                    ("qualified-name", 0.95)
                } else {
                    ("exact-match", 0.9)
                };
                Some(EdgeRecord {
                    source: site.from_id.clone(),
                    target: target_id.clone(),
                    kind: "imports".to_string(),
                    metadata: Some(metadata_json(conf, by)),
                    line: Some(site.line),
                    col: Some(site.col),
                    provenance: None,
                })
            }
            SitePayload::Inherit { parent, edge_kind } => {
                let kinds = if *edge_kind == "implements" {
                    IMPLEMENTS_KINDS
                } else {
                    EXTENDS_KINDS
                };
                let r = self.resolve_name(from_file, parent, kinds)?;
                Some(self.edge(site, edge_kind, &r))
            }
            SitePayload::InheritAuto { parent } => {
                let r = self.resolve_name(from_file, parent, INHERIT_ANY_KINDS)?;
                // CodeGraph classifies a conformed interface/protocol as
                // `implements` and a class/struct base as `extends`.
                let edge_kind = match r.node.kind.as_str() {
                    "interface" | "trait" => "implements",
                    _ => "extends",
                };
                Some(self.edge(site, edge_kind, &r))
            }
        }
    }

    /// Resolve a plain `name` to one node of an allowed `kind`, by precedence:
    /// same-file → import-target → unique-global → same-dir → same-language.
    /// Ambiguous-after-all-tiers resolves to `None` (dropped).
    fn resolve_name(&self, from_file: &str, name: &str, kinds: &[&str]) -> Option<Resolved<'a>> {
        let cands: Vec<usize> = self
            .by_name
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| kinds.contains(&self.nodes[i].kind.as_str()))
            .collect();
        if cands.is_empty() {
            return None;
        }
        let global_unique = cands.len() == 1;
        // A globally-unique name is high-confidence even across files; an
        // ambiguous one that we still pin down (same-file/import) is low.
        let base_conf = if global_unique { 0.9 } else { 0.4 };

        // 1. same-file
        if let Some(i) = self.pick(&cands, |n| n.file_path == from_file) {
            return Some(Resolved {
                node: &self.nodes[i],
                resolved_by: "exact-match",
                confidence: base_conf,
            });
        }
        // 2. import-target
        if let Some(imports) = self.imported_files.get(from_file) {
            if let Some(i) = self.pick(&cands, |n| imports.iter().any(|f| f == &n.file_path)) {
                return Some(Resolved {
                    node: &self.nodes[i],
                    resolved_by: "import",
                    confidence: 0.9,
                });
            }
        }
        // The remaining tiers never cross a language boundary: a Rust call must
        // not resolve to a same-named TypeScript function. Restrict to the
        // caller's language (same-file/import tiers above are already
        // language-consistent, so this only tightens the global fallbacks).
        let caller_lang = self.language_of(from_file);
        let lang_cands: Vec<usize> = match caller_lang {
            Some(lang) => cands
                .iter()
                .copied()
                .filter(|&i| self.nodes[i].language == lang)
                .collect(),
            None => cands.clone(),
        };
        // 3. unique within the caller's language
        if lang_cands.len() == 1 {
            return Some(Resolved {
                node: &self.nodes[lang_cands[0]],
                resolved_by: "exact-match",
                // High confidence only when it was also globally unique.
                confidence: if global_unique { 0.9 } else { 0.4 },
            });
        }
        // 4. same-directory, if unique there (within the language)
        let dir = parent_dir(from_file);
        let same_dir: Vec<usize> = lang_cands
            .iter()
            .copied()
            .filter(|&i| parent_dir(&self.nodes[i].file_path) == dir)
            .collect();
        if same_dir.len() == 1 {
            return Some(Resolved {
                node: &self.nodes[same_dir[0]],
                resolved_by: "exact-match",
                confidence: 0.5,
            });
        }
        // Still ambiguous → drop (precision over recall).
        None
    }

    /// The `language` recorded for `file_path` (O(1) via [`Self::file_to_lang`]).
    fn language_of(&self, file_path: &str) -> Option<&'a str> {
        self.file_to_lang.get(file_path).copied()
    }

    /// Resolve an explicit `qualified_name` (a Rust `A::b` path call, or a method
    /// call whose receiver type was inferred). Candidates are **language-scoped**
    /// to the caller's file first — a qualified name that collides across
    /// languages (e.g. Rust `Point::dist` and TS `Point::dist`) must never
    /// resolve a Rust call to the TypeScript symbol. If the caller's language has
    /// no matching definition we **drop** (precision over recall), rather than
    /// falling back to a foreign symbol. Within the caller's language, ties break
    /// same-file → import-target → lexicographically.
    fn resolve_qualified(&self, from_file: &str, qn: &str, kinds: &[&str]) -> Option<Resolved<'a>> {
        let caller_lang = self.language_of(from_file);
        let cands: Vec<usize> = self
            .by_qn
            .get(qn)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| kinds.contains(&self.nodes[i].kind.as_str()))
            .filter(|&i| caller_lang.is_none_or(|l| self.nodes[i].language == l))
            .collect();
        if cands.is_empty() {
            return None;
        }
        let i = self
            .pick(&cands, |n| n.file_path == from_file)
            .or_else(|| {
                self.imported_files.get(from_file).and_then(|imports| {
                    self.pick(&cands, |n| imports.iter().any(|f| f == &n.file_path))
                })
            })
            .unwrap_or_else(|| {
                // Lexicographically smallest file for a stable tie-break.
                *cands
                    .iter()
                    .min_by(|&&a, &&b| self.nodes[a].file_path.cmp(&self.nodes[b].file_path))
                    .unwrap()
            });
        Some(Resolved {
            node: &self.nodes[i],
            resolved_by: "qualified-name",
            confidence: 0.95,
        })
    }

    /// Pick the candidate (lowest index for stability, then smallest file path)
    /// satisfying `pred`, if any.
    fn pick(&self, cands: &[usize], pred: impl Fn(&NodeRecord) -> bool) -> Option<usize> {
        cands
            .iter()
            .copied()
            .filter(|&i| pred(&self.nodes[i]))
            .min_by(|&a, &b| {
                self.nodes[a].file_path.cmp(&self.nodes[b].file_path).then(
                    self.nodes[a]
                        .qualified_name
                        .cmp(&self.nodes[b].qualified_name),
                )
            })
    }

    /// The `file_path` owning `node_id` (O(1) via [`Self::id_to_file`]).
    fn file_of(&self, node_id: &str) -> Option<&'a str> {
        self.id_to_file.get(node_id).copied()
    }

    fn edge(&self, site: &RawSite, kind: &str, r: &Resolved<'a>) -> EdgeRecord {
        self.edge_meta(site, kind, r.node, r.resolved_by, r.confidence)
    }

    fn edge_meta(
        &self,
        site: &RawSite,
        kind: &str,
        target: &NodeRecord,
        resolved_by: &str,
        confidence: f64,
    ) -> EdgeRecord {
        EdgeRecord {
            source: site.from_id.clone(),
            target: target.id.clone(),
            kind: kind.to_string(),
            metadata: Some(metadata_json(confidence, resolved_by)),
            line: Some(site.line),
            col: Some(site.col),
            provenance: None,
        }
    }
}

/// The `edges.metadata` JSON CodeGraph writes: `{"confidence":x,"resolvedBy":s}`.
///
/// Emitted with keys in this fixed order (which also happens to be alphabetical,
/// matching `serde_json`'s default map ordering and the golden fixture).
fn metadata_json(confidence: f64, resolved_by: &str) -> String {
    format!("{{\"confidence\":{confidence},\"resolvedBy\":\"{resolved_by}\"}}")
}

/// The parent-directory portion of a forward-slash stored path (`""` for a
/// top-level file).
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Try to map an import's module string, as written in `from_file`, to a
/// concrete file path present in the graph.
///
/// Handles the tier-1 import forms: TS/JS relative specifiers (`./geometry`),
/// Python dotted modules resolved as sibling files (`shapes`, `pkg.mod`), and
/// Rust sibling modules (`use geometry` next to `mod geometry;`). External
/// modules (`asyncio`, `enum`, `react`) resolve to `None`.
fn resolve_module_to_file(from_file: &str, module: &str, files: &HashSet<&str>) -> Option<String> {
    const EXTS: &[&str] = &["rs", "py", "pyi", "ts", "tsx", "js", "jsx", "mts", "cts"];
    let dir = parent_dir(from_file);

    // Normalize the module into a slash path relative to `dir`.
    let rel: String = if let Some(stripped) = module.strip_prefix("./") {
        stripped.to_string()
    } else if module.starts_with("../") {
        // Collapse `../` against `dir` below via join_relative.
        module.to_string()
    } else if module.starts_with('.') {
        // A leading bare '.' (rare) — treat the remainder as relative.
        module.trim_start_matches('.').to_string()
    } else {
        // Dotted (Python `a.b`) or plain module name → path segments.
        module.replace('.', "/")
    };

    let base = join_relative(dir, &rel);

    // Try `<base>.<ext>` and `<base>/mod.rs` | `<base>/index.ts` style entries.
    let mut candidates: Vec<String> = Vec::new();
    for ext in EXTS {
        candidates.push(format!("{base}.{ext}"));
    }
    candidates.push(format!("{base}/mod.rs"));
    candidates.push(format!("{base}/index.ts"));
    candidates.push(format!("{base}/index.tsx"));
    candidates.push(format!("{base}/index.js"));
    candidates.push(format!("{base}/__init__.py"));

    candidates.into_iter().find(|c| files.contains(c.as_str()))
}

/// Resolve a C/C++ `#include` header path to a concrete file in the graph.
///
/// Tries the header relative to the including file's directory, then by basename
/// anywhere in the graph (CodeGraph resolves local includes by name). System
/// includes (`<math.h>` for headers not in the tree) return `None`.
fn resolve_include_to_file(from_file: &str, header: &str, files: &HashSet<&str>) -> Option<String> {
    let dir = parent_dir(from_file);
    let joined = join_relative(dir, header);
    if files.contains(joined.as_str()) {
        return Some(joined);
    }
    // Fall back to a unique basename match anywhere in the graph.
    let base = header.rsplit('/').next().unwrap_or(header);
    let matches: Vec<&str> = files
        .iter()
        .copied()
        .filter(|f| f.rsplit('/').next() == Some(base))
        .collect();
    if matches.len() == 1 {
        return Some(matches[0].to_string());
    }
    None
}

/// Join a forward-slash `rel` onto `dir`, resolving leading `../` segments.
fn join_relative(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Best-effort extraction of a bare type name from a Rust/TS type-annotation
/// fragment: strip references, `mut`, whitespace, and any generic/tuple tail.
/// Used by the parser's local type inference. Returns `None` for empty input.
pub(crate) fn bare_type_name(mut ty: &str) -> Option<String> {
    ty = ty.trim();
    ty = ty.trim_start_matches('&').trim();
    if let Some(rest) = ty.strip_prefix("mut ") {
        ty = rest.trim();
    }
    // Take up to the first generic/tuple/path boundary.
    let end = ty
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(ty.len());
    let name = &ty[..end];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_relative_resolves_parent_segments() {
        assert_eq!(
            join_relative("typescript", "./geometry"),
            "typescript/geometry"
        );
        assert_eq!(join_relative("a/b", "../c"), "a/c");
        assert_eq!(join_relative("", "shapes"), "shapes");
    }

    #[test]
    fn bare_type_name_strips_refs_and_generics() {
        assert_eq!(bare_type_name("&Point").as_deref(), Some("Point"));
        assert_eq!(bare_type_name("&mut Shape").as_deref(), Some("Shape"));
        assert_eq!(
            bare_type_name("Vec<(Scalar, Scalar)>").as_deref(),
            Some("Vec")
        );
        assert_eq!(bare_type_name("  Scalar  ").as_deref(), Some("Scalar"));
        assert_eq!(bare_type_name(""), None);
    }

    #[test]
    fn module_resolves_relative_and_sibling() {
        let files: HashSet<&str> = [
            "typescript/geometry.ts",
            "python/shapes.py",
            "rust/geometry.rs",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            resolve_module_to_file("typescript/index.ts", "./geometry", &files).as_deref(),
            Some("typescript/geometry.ts")
        );
        assert_eq!(
            resolve_module_to_file("python/main.py", "shapes", &files).as_deref(),
            Some("python/shapes.py")
        );
        assert_eq!(
            resolve_module_to_file("rust/lib.rs", "geometry", &files).as_deref(),
            Some("rust/geometry.rs")
        );
        // External module → unresolved.
        assert_eq!(
            resolve_module_to_file("python/main.py", "asyncio", &files),
            None
        );
    }

    // ---- cross-language resolution guard (rev-21 blocking finding) ----------

    fn node(kind: &str, name: &str, qn: &str, file: &str, lang: &str) -> NodeRecord {
        NodeRecord::new(kind, name, qn, file, lang, 1, 3, 0, 0, 0)
    }

    fn method_call_site(from_id: &str, recv_type: &str, name: &str) -> RawSite {
        RawSite {
            from_id: from_id.to_string(),
            line: 2,
            col: 4,
            payload: SitePayload::MethodCall {
                recv_type: Some(recv_type.to_string()),
                name: name.to_string(),
            },
        }
    }

    /// A qualified/method call must NEVER resolve across a language boundary,
    /// even when the `qualified_name` collides. A Rust `p.dist()` (receiver typed
    /// `Point`) with no Rust `Point::dist` must DROP, not fall through to a
    /// TypeScript `Point::dist` via the lexicographic tie-break. The golden
    /// fixture has no such collision, so this case is constructed explicitly.
    #[test]
    fn qualified_call_never_resolves_across_languages() {
        let go = node("function", "go", "go", "go.rs", "rust");
        let go_id = go.id.clone();
        let base = vec![
            node("file", "go.rs", "go.rs", "go.rs", "rust"),
            node("file", "geo.ts", "geo.ts", "geo.ts", "typescript"),
            go,
            node("class", "Point", "Point", "geo.ts", "typescript"),
            // A TypeScript Point::dist — the ONLY definition of that qualified name.
            node("method", "dist", "Point::dist", "geo.ts", "typescript"),
        ];

        // Rust caller: p.dist() with p: Point, but there is no *Rust* Point::dist.
        let edges =
            SymbolTable::build(&base).resolve_all(&[method_call_site(&go_id, "Point", "dist")]);
        assert!(
            edges.is_empty(),
            "a Rust call must not resolve to a TypeScript Point::dist: {edges:?}"
        );

        // Positive control: add a Rust Point::dist → now it resolves, and to the
        // Rust one (same language), never the TypeScript collision.
        let mut with_rust = base.clone();
        with_rust.push(node("method", "dist", "Point::dist", "go.rs", "rust"));
        let edges = SymbolTable::build(&with_rust)
            .resolve_all(&[method_call_site(&go_id, "Point", "dist")]);
        assert_eq!(edges.len(), 1, "should resolve within Rust");
        assert_eq!(edges[0].kind, "calls");
        let target = with_rust.iter().find(|n| n.id == edges[0].target).unwrap();
        assert_eq!(target.file_path, "go.rs");
        assert_eq!(target.language, "rust");
    }

    /// The same guard for an explicit Rust path call `Point::new(..)`: a bare
    /// qualified call to a name that exists only in another language drops.
    #[test]
    fn explicit_qualified_path_call_is_language_scoped() {
        let caller = node("function", "make", "make", "a.rs", "rust");
        let caller_id = caller.id.clone();
        let nodes = vec![
            node("file", "a.rs", "a.rs", "a.rs", "rust"),
            node("file", "b.ts", "b.ts", "b.ts", "typescript"),
            caller,
            node("method", "new", "Widget::new", "b.ts", "typescript"),
        ];
        let site = RawSite {
            from_id: caller_id,
            line: 2,
            col: 4,
            payload: SitePayload::QualifiedCall {
                qualifier: "Widget".to_string(),
                name: "new".to_string(),
            },
        };
        let edges = SymbolTable::build(&nodes).resolve_all(&[site]);
        assert!(
            edges.is_empty(),
            "Rust Widget::new() must not resolve to a TS Widget::new: {edges:?}"
        );
    }

    /// A qualified call to an enum tuple-variant constructor (`Error::Sqlite(e)`)
    /// resolves to the `enum_member` as a `calls` edge (matching CodeGraph).
    #[test]
    fn qualified_call_resolves_enum_variant_constructor() {
        let caller = node("function", "f", "f", "a.rs", "rust");
        let caller_id = caller.id.clone();
        let variant = node("enum_member", "Sqlite", "Error::Sqlite", "a.rs", "rust");
        let variant_id = variant.id.clone();
        let nodes = vec![
            node("file", "a.rs", "a.rs", "a.rs", "rust"),
            caller,
            variant,
        ];
        let site = RawSite {
            from_id: caller_id,
            line: 2,
            col: 0,
            payload: SitePayload::QualifiedCall {
                qualifier: "Error".to_string(),
                name: "Sqlite".to_string(),
            },
        };
        let edges = SymbolTable::build(&nodes).resolve_all(&[site]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "calls");
        assert_eq!(edges[0].target, variant_id);
    }

    /// A method call whose receiver type could NOT be inferred is **dropped**,
    /// never resolved by method name — even when the name is project-unique. A
    /// bare name routinely collides with a std/library method of the same name,
    /// so name-based resolution fabricates (rev-37 blocker 1). This also covers
    /// the two-methods-one-file case (rev-37 blocker 2): an un-inferrable receiver
    /// must not resolve to the wrong of two same-named methods.
    #[test]
    fn un_inferrable_method_receiver_drops_never_resolves_by_name() {
        let caller = node("method", "run", "Mcp::run", "mcp.rs", "rust");
        let caller_id = caller.id.clone();
        let site = |name: &str| RawSite {
            from_id: caller_id.clone(),
            line: 2,
            col: 0,
            payload: SitePayload::MethodCall {
                recv_type: None,
                name: name.to_string(),
            },
        };

        // Project-unique method name (`as_str`) — the real receiver at an
        // un-inferrable site could be a std `String`, so we must NOT fabricate an
        // edge to the sole local `GpuMode::as_str`.
        let unique = vec![
            node("file", "mcp.rs", "mcp.rs", "mcp.rs", "rust"),
            node("file", "gpu.rs", "gpu.rs", "gpu.rs", "rust"),
            caller.clone(),
            node("method", "as_str", "GpuMode::as_str", "gpu.rs", "rust"),
        ];
        assert!(
            SymbolTable::build(&unique)
                .resolve_all(&[site("as_str")])
                .is_empty(),
            "un-inferrable `.as_str()` must drop, not resolve to the sole local as_str"
        );

        // Two same-named methods in ONE file (blocker 2): an un-inferrable
        // receiver must not pick either A::m or B::m.
        let two_in_one = vec![
            node("file", "mcp.rs", "mcp.rs", "mcp.rs", "rust"),
            caller,
            node("method", "m", "A::m", "mcp.rs", "rust"),
            node("method", "m", "B::m", "mcp.rs", "rust"),
        ];
        assert!(
            SymbolTable::build(&two_in_one)
                .resolve_all(&[site("m")])
                .is_empty(),
            "un-inferrable `.m()` with A::m and B::m in one file must drop"
        );
    }

    /// Positive control: when the receiver type IS inferred, the method resolves
    /// to `Type::name` (this is the only path that emits an instance-method call).
    #[test]
    fn inferred_receiver_resolves_qualified_method() {
        let caller = node("method", "run", "Mcp::run", "mcp.rs", "rust");
        let caller_id = caller.id.clone();
        let target = node("method", "callees", "GraphDb::callees", "db.rs", "rust");
        let target_id = target.id.clone();
        let nodes = vec![
            node("file", "mcp.rs", "mcp.rs", "mcp.rs", "rust"),
            node("file", "db.rs", "db.rs", "db.rs", "rust"),
            caller,
            target,
        ];
        // recv_type inferred as GraphDb → resolves GraphDb::callees.
        let edges = SymbolTable::build(&nodes)
            .resolve_all(&[method_call_site(&caller_id, "GraphDb", "callees")]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "calls");
        assert_eq!(edges[0].target, target_id);
    }

    /// `Inherit` sites resolve to `extends`/`implements` edges against the right
    /// kinds and are language-scoped.
    #[test]
    fn inherit_sites_resolve_to_extends_and_implements() {
        let circle = node("struct", "Circle", "Circle", "a.rs", "rust");
        let circle_id = circle.id.clone();
        let shape = node("trait", "Shape", "Shape", "a.rs", "rust");
        let shape_id = shape.id.clone();
        let nodes = vec![node("file", "a.rs", "a.rs", "a.rs", "rust"), circle, shape];
        let site = RawSite {
            from_id: circle_id,
            line: 5,
            col: 0,
            payload: SitePayload::Inherit {
                parent: "Shape".to_string(),
                edge_kind: "implements",
            },
        };
        let edges = SymbolTable::build(&nodes).resolve_all(&[site]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "implements");
        assert_eq!(edges[0].target, shape_id);

        // A bound to a name with no local type-like definition drops (a `Send`
        // supertrait resolves to nothing rather than being fabricated).
        let src2 = node("trait", "Named", "Named", "a.rs", "rust");
        let src2_id = src2.id.clone();
        let nodes2 = vec![node("file", "a.rs", "a.rs", "a.rs", "rust"), src2];
        let site2 = RawSite {
            from_id: src2_id,
            line: 1,
            col: 0,
            payload: SitePayload::Inherit {
                parent: "Send".to_string(),
                edge_kind: "extends",
            },
        };
        assert!(SymbolTable::build(&nodes2).resolve_all(&[site2]).is_empty());
    }
}
