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
//! resolve against `qualified_name` directly. Method calls resolve only when the
//! receiver's type was inferred locally; an un-inferrable receiver is dropped
//! rather than guessed.
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
    TypeRef { name: String },
    /// An import statement: a file → import-node edge. `module` is the imported
    /// module string (used only for metadata / module→file resolution).
    Import { target_id: String, module: String },
}

/// A resolved target plus how it was found (for the edge's `metadata`).
struct Resolved<'a> {
    node: &'a NodeRecord,
    resolved_by: &'static str,
    confidence: f64,
}

/// Node kinds that a bare/qualified *call* may target.
const CALLABLE_KINDS: &[&str] = &["function", "method"];
/// Node kinds a constructor/`new` may target.
const CLASS_KINDS: &[&str] = &["class", "struct", "interface"];
/// Node kinds a type reference may target.
const TYPE_KINDS: &[&str] = &["type_alias", "struct", "enum", "class", "interface"];

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

        for (i, n) in nodes.iter().enumerate() {
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
                let r = self.resolve_qualified(from_file, &qn, CALLABLE_KINDS)?;
                Some(self.edge_meta(site, "calls", r.node, "qualified-name", 0.95))
            }
            SitePayload::MethodCall { recv_type, name } => {
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
            SitePayload::Import { target_id, module } => {
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

    /// The `language` recorded for `file_path` (from any of its nodes).
    fn language_of(&self, file_path: &str) -> Option<&'a str> {
        self.nodes
            .iter()
            .find(|n| n.file_path == file_path)
            .map(|n| n.language.as_str())
    }

    /// Resolve an explicit `qualified_name`. Qualified calls are unambiguous by
    /// construction, so on a tie we still pick deterministically (same-file →
    /// import → lexicographic) rather than dropping.
    fn resolve_qualified(&self, from_file: &str, qn: &str, kinds: &[&str]) -> Option<Resolved<'a>> {
        let cands: Vec<usize> = self
            .by_qn
            .get(qn)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| kinds.contains(&self.nodes[i].kind.as_str()))
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

    fn file_of(&self, node_id: &str) -> Option<&'a str> {
        self.nodes
            .iter()
            .find(|n| n.id == node_id)
            .map(|n| n.file_path.as_str())
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
}
