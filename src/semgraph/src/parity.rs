//! Parity harness core: quantify how closely a **semgraph**-built graph matches
//! a **CodeGraph 0.9.7**-built one (issue #78, Phase 2 item 6 / "P2c").
//!
//! This module is the pure, testable engine behind the `parity` binary
//! (`src/bin/parity.rs`). It reads two schema-v4 `codegraph.db` files, diffs
//! them, applies a committed *whitelist* of known-better deviations
//! (ADR-003/004), and produces a machine-readable [`ParityReport`] with per-kind
//! / per-language match percentages plus missing/extra listings.
//!
//! ## What is compared
//!
//! - **Nodes** are matched in two passes. First on `(kind, qualified_name,
//!   file_path)` (plus `(start_line, end_line)` when
//!   [`CompareOptions::strict_line_range`] is set). The **residuals** — nodes
//!   present on only one side after the exact pass — are then matched a second
//!   time on physical identity `(kind, file_path, start_line, end_line)`,
//!   ignoring the qualified name. A residual pair that matches there is a
//!   **reconvention**: the *same physical definition* recorded under a different
//!   qualified-name *convention* (e.g. CodeGraph omits Rust's `tests::` module
//!   prefix, or names a nested Python function differently). Reconventions are
//!   the same node — they count toward recall — but are reported in their own
//!   category, showing both qn forms, so a convention gap is never conflated
//!   with a genuine one.
//! - **Edges** are keyed on `(source_qn, target_qn, kind)` as a **multiset**.
//!   Because that key is qualified-name-based, the exact same convention drift
//!   inflates the edge miss. So before matching, every golden edge endpoint's qn
//!   is **translated** through the reconvention map (golden-qn → semgraph-qn),
//!   giving *convention-matched* edge parity: the surviving miss is the genuine
//!   resolution gap, not naming noise. A raw (untranslated) `calls` percentage is
//!   also reported so the size of the convention effect is visible.
//! - **Node attributes** (`is_async`, `docstring`, `signature`, and, in the
//!   default relaxed mode, the line-range) are compared for every node matched on
//!   the relaxed key and reported; divergences can be whitelisted, but only in
//!   the documented-better *direction* (see below).
//!
//! ## Known-better deltas (the whitelist)
//!
//! ADR-003/004 record deliberate improvements over CodeGraph 0.9.7 that must
//! **not** count as failures: `is_async` correctness, docstring cleanups, and
//! the omission of CodeGraph's duplicate return-type `references`. These live in
//! a committed [`Whitelist`] with a per-entry justification and a **direction**,
//! so forgiving "semgraph adds a docstring" never also forgives its regression
//! ("semgraph *drops* a docstring").

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{Error, Result};

/// A node projected for parity comparison — the schema-v4 columns the diff and
/// the attribute checks consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PNode {
    pub kind: String,
    pub qualified_name: String,
    pub file_path: String,
    pub language: String,
    pub start_line: i64,
    pub end_line: i64,
    pub is_async: bool,
    pub docstring: Option<String>,
    pub signature: Option<String>,
}

impl PNode {
    /// The exact match key: `(kind, qualified_name, file_path)`, plus the line
    /// range when `strict`.
    fn exact_key(&self, strict: bool) -> NodeKey {
        (
            self.kind.clone(),
            self.qualified_name.clone(),
            self.file_path.clone(),
            strict.then_some((self.start_line, self.end_line)),
        )
    }
    /// Physical identity used for the reconvention second pass and for the
    /// attribute pairing: `(kind, file_path, start_line, end_line)`, ignoring qn.
    fn identity(&self) -> Identity {
        (
            self.kind.clone(),
            self.file_path.clone(),
            self.start_line,
            self.end_line,
        )
    }
}

type NodeKey = (String, String, String, Option<(i64, i64)>);
type Identity = (String, String, i64, i64);

/// An edge projected for parity comparison. Language is the **caller's** (source
/// node's) language, matching ADR-004's language-scoped resolution. The endpoint
/// `(kind, file)` pairs let the reconvention map be applied per endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PEdge {
    pub source_qn: String,
    pub target_qn: String,
    pub kind: String,
    pub language: String,
    pub source_kind: String,
    pub source_file: String,
    pub target_kind: String,
    pub target_file: String,
}

type EdgeKey = (String, String, String);

/// Reconvention translation: `(endpoint_kind, endpoint_file, golden_qn)` →
/// `semgraph_qn`. Applied to golden edge endpoints so a convention-only qn
/// difference doesn't read as a missing edge.
type Translation = HashMap<(String, String, String), String>;

/// The two projections read out of one `codegraph.db`.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: Vec<PNode>,
    pub edges: Vec<PEdge>,
}

/// Read the parity projection (nodes + edges) out of a schema-v4 `codegraph.db`.
///
/// Works on any such database, whether produced by CodeGraph or by
/// [`crate::index_roots`].
pub fn extract_parity(db_path: &Path) -> Result<Graph> {
    let conn = Connection::open(db_path).map_err(|source| Error::Open {
        path: db_path.display().to_string(),
        source,
    })?;

    let mut nstmt = conn.prepare(
        "SELECT kind, qualified_name, file_path, COALESCE(language,''), \
                COALESCE(start_line,0), COALESCE(end_line,0), \
                COALESCE(is_async,0), docstring, signature \
         FROM nodes",
    )?;
    let nodes = nstmt
        .query_map([], |r| {
            Ok(PNode {
                kind: r.get(0)?,
                qualified_name: r.get(1)?,
                file_path: r.get(2)?,
                language: r.get(3)?,
                start_line: r.get(4)?,
                end_line: r.get(5)?,
                is_async: r.get::<_, i64>(6)? != 0,
                docstring: normalize_opt(r.get::<_, Option<String>>(7)?),
                signature: normalize_opt(r.get::<_, Option<String>>(8)?),
            })
        })?
        .filter_map(std::result::Result::ok)
        .collect();

    let mut estmt = conn.prepare(
        "SELECT s.qualified_name, t.qualified_name, e.kind, COALESCE(s.language,''), \
                s.kind, s.file_path, t.kind, t.file_path \
         FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target",
    )?;
    let edges = estmt
        .query_map([], |r| {
            Ok(PEdge {
                source_qn: r.get(0)?,
                target_qn: r.get(1)?,
                kind: r.get(2)?,
                language: r.get(3)?,
                source_kind: r.get(4)?,
                source_file: r.get(5)?,
                target_kind: r.get(6)?,
                target_file: r.get(7)?,
            })
        })?
        .filter_map(std::result::Result::ok)
        .collect();

    Ok(Graph { nodes, edges })
}

/// An empty string in a nullable text column is treated as "absent" so that
/// CodeGraph's `NULL` docstring and semgraph's `""` don't read as a difference.
fn normalize_opt(v: Option<String>) -> Option<String> {
    match v {
        Some(s) if s.is_empty() => None,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Whitelist
// ---------------------------------------------------------------------------

/// A committed set of *known-better* deviations from CodeGraph 0.9.7 (ADR-003/
/// ADR-004) that must be counted separately rather than as parity failures.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Whitelist {
    /// Node-attribute deltas that are expected and better (e.g. `is_async`).
    #[serde(default)]
    pub node_attrs: Vec<NodeAttrRule>,
    /// Missing/extra edge instances that are expected (e.g. CodeGraph's
    /// duplicate return-type references).
    #[serde(default)]
    pub edges: Vec<EdgeRule>,
    /// Missing/extra whole nodes that are expected.
    #[serde(default)]
    pub nodes: Vec<NodeRule>,
}

fn star() -> String {
    "*".to_string()
}

/// A whitelist rule for a node-attribute delta.
///
/// A rule matches only in the documented-better **direction**: the `direction`
/// field constrains which side's value the improvement must be on, so forgiving
/// "semgraph adds an `is_async`/docstring CodeGraph missed" does NOT also forgive
/// the regression where semgraph *loses* one.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeAttrRule {
    /// Attribute name: `is_async` | `docstring` | `signature` | `line_range`.
    pub attr: String,
    #[serde(default = "star")]
    pub qualified_name: String,
    #[serde(default = "star")]
    pub file: String,
    /// Optional exact language filter (`rust`/`python`/`typescript`/…).
    #[serde(default)]
    pub language: Option<String>,
    /// Which direction of divergence is forgiven. One of:
    /// `semgraph_true` / `semgraph_false` (bool attrs like `is_async`),
    /// `semgraph_nonempty` / `codegraph_empty` (text attrs like `docstring`).
    /// Absent = any direction (discouraged; only for symmetric cosmetic attrs).
    #[serde(default)]
    pub direction: Option<String>,
    pub justification: String,
}

impl NodeAttrRule {
    /// Whether this rule forgives a delta whose semgraph value is `ours` and
    /// CodeGraph value is `golden` (both rendered as strings).
    fn direction_ok(&self, ours: &str, golden: &str) -> bool {
        match self.direction.as_deref() {
            None => true,
            Some("semgraph_true") => ours == "true",
            Some("semgraph_false") => ours == "false",
            Some("semgraph_nonempty") => !ours.is_empty(),
            Some("codegraph_empty") => golden.is_empty(),
            // Unknown direction is fail-closed: never forgive.
            Some(_) => false,
        }
    }
}

/// A whitelist rule for a missing/extra edge instance.
#[derive(Debug, Clone, Deserialize)]
pub struct EdgeRule {
    /// `missing` (golden has it, we don't) or `extra` (we emit it, golden doesn't).
    pub side: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "star")]
    pub source: String,
    #[serde(default = "star")]
    pub target: String,
    /// When true, only whitelist a *missing* instance whose key we still emit at
    /// least once (i.e. a duplicate-multiplicity omission, not a true recall gap).
    #[serde(default)]
    pub only_duplicates: bool,
    pub justification: String,
}

/// A whitelist rule for a missing/extra whole node.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeRule {
    pub side: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "star")]
    pub qualified_name: String,
    #[serde(default = "star")]
    pub file: String,
    pub justification: String,
}

impl Whitelist {
    /// Parse a whitelist from JSON text.
    pub fn from_json(text: &str) -> Result<Whitelist> {
        serde_json::from_str(text).map_err(|e| Error::Invalid {
            path: "<whitelist>".to_string(),
            detail: format!("invalid parity whitelist JSON: {e}"),
        })
    }

    /// Load a whitelist from a file path.
    pub fn load(path: &Path) -> Result<Whitelist> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Invalid {
            path: path.display().to_string(),
            detail: format!("cannot read whitelist: {e}"),
        })?;
        Whitelist::from_json(&text)
    }

    fn node_attr_justification(
        &self,
        attr: &str,
        n: &PNode,
        ours_val: &str,
        golden_val: &str,
    ) -> Option<&str> {
        self.node_attrs.iter().find_map(|r| {
            (r.attr == attr
                && glob_match(&r.qualified_name, &n.qualified_name)
                && glob_match(&r.file, &n.file_path)
                && r.language.as_deref().is_none_or(|l| l == n.language)
                && r.direction_ok(ours_val, golden_val))
            .then_some(r.justification.as_str())
        })
    }

    fn edge_justification(&self, side: &str, e: &EdgeKey, our_count_for_key: i64) -> Option<&str> {
        let (src, tgt, kind) = e;
        self.edges.iter().find_map(|r| {
            (r.side == side
                && r.kind.as_deref().is_none_or(|k| k == kind)
                && glob_match(&r.source, src)
                && glob_match(&r.target, tgt)
                && (!r.only_duplicates || our_count_for_key >= 1))
                .then_some(r.justification.as_str())
        })
    }

    fn node_justification(&self, side: &str, n: &PNode) -> Option<&str> {
        self.nodes.iter().find_map(|r| {
            (r.side == side
                && r.kind.as_deref().is_none_or(|k| k == n.kind)
                && glob_match(&r.qualified_name, &n.qualified_name)
                && glob_match(&r.file, &n.file_path))
            .then_some(r.justification.as_str())
        })
    }
}

/// Minimal glob: `*` matches any run (including empty); every other character is
/// literal. `"*"` alone matches anything.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            return text[pos..].ends_with(part);
        } else {
            match text[pos..].find(part) {
                Some(off) => pos += off + part.len(),
                None => return false,
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Options controlling [`compare`].
#[derive(Debug, Clone, Default)]
pub struct CompareOptions {
    /// Pin `(start_line, end_line)` into the exact node key. Off by default; when
    /// off, line-range is compared as a whitelistable attribute instead.
    pub strict_line_range: bool,
}

/// Per-`kind` (or per-language) match counts for one entity type.
#[derive(Debug, Clone, Default)]
pub struct GroupStat {
    pub label: String,
    pub golden: i64,
    /// Golden items matched exactly (same key).
    pub matched: i64,
    /// Golden items matched only after normalizing the qualified-name
    /// convention (nodes: reconvention second pass). Counts toward recall.
    pub reconvention: i64,
    pub missing: i64,
    pub extra: i64,
    pub whitelisted_missing: i64,
    pub whitelisted_extra: i64,
}

impl GroupStat {
    /// Recall percentage after crediting reconventions and whitelisted
    /// omissions. Returns `None` when there is nothing to measure (no golden
    /// items) — callers must decide how to treat "N/A", never silently 100%.
    pub fn match_pct_opt(&self) -> Option<f64> {
        if self.golden == 0 {
            return None;
        }
        let effective_missing = (self.missing - self.whitelisted_missing).max(0);
        Some(100.0 * (self.golden - effective_missing) as f64 / self.golden as f64)
    }

    /// Convenience for display: `match_pct_opt` or 100.0 when there is no data.
    pub fn match_pct(&self) -> f64 {
        self.match_pct_opt().unwrap_or(100.0)
    }
}

/// A single reported diff item, with its whitelist justification when one
/// applies.
#[derive(Debug, Clone)]
pub struct DiffItem {
    pub description: String,
    pub whitelisted: bool,
    pub justification: Option<String>,
}

/// The full parity result.
#[derive(Debug, Clone)]
pub struct ParityReport {
    pub node_total: GroupStat,
    pub edge_total: GroupStat,
    pub nodes_by_kind: Vec<GroupStat>,
    pub nodes_by_language: Vec<GroupStat>,
    pub edges_by_kind: Vec<GroupStat>,
    pub edges_by_language: Vec<GroupStat>,
    pub missing_nodes: Vec<DiffItem>,
    pub extra_nodes: Vec<DiffItem>,
    /// Same-physical-node, different-qn-convention pairs (nodes credited to
    /// recall but reported separately, showing both qn forms).
    pub reconvention_nodes: Vec<DiffItem>,
    pub missing_edges: Vec<DiffItem>,
    pub extra_edges: Vec<DiffItem>,
    pub attr_deltas: Vec<DiffItem>,
    /// `calls` recall computed on the **raw** qualified-name key (no
    /// reconvention translation), for showing how much of the calls gap was
    /// naming convention. `None` when the golden side has no `calls` edges.
    pub calls_pct_raw: Option<f64>,
}

impl ParityReport {
    /// Overall node recall after reconvention + whitelist (the `--min-nodes`
    /// metric). Nodes always have data in a real comparison; falls back to 100
    /// only for an empty tree.
    pub fn node_match_pct(&self) -> f64 {
        self.node_total.match_pct()
    }

    /// Convention-matched `calls`-edge recall (the `--min-calls` metric).
    /// `None` when the golden side has no `calls` edges — the gate treats that
    /// as vacuously satisfied rather than as a silent 100%.
    pub fn calls_match_pct(&self) -> Option<f64> {
        self.edges_by_kind
            .iter()
            .find(|g| g.label == "calls")
            .and_then(GroupStat::match_pct_opt)
    }

    /// Whether the report clears both acceptance thresholds. The node gate
    /// always applies; the calls gate is vacuously satisfied when there are no
    /// golden `calls` edges. Both compare **raw** (unrounded) percentages.
    pub fn passes(&self, min_nodes: f64, min_calls: f64) -> bool {
        let nodes_ok = self.node_match_pct() >= min_nodes;
        let calls_ok = self.calls_match_pct().is_none_or(|p| p >= min_calls);
        nodes_ok && calls_ok
    }

    /// Machine-readable summary. Percentages are **raw** (unrounded) so a
    /// consumer gating on them agrees with [`passes`]; render rounded for humans.
    pub fn to_json(&self) -> Value {
        let grp = |g: &GroupStat| {
            json!({
                "label": g.label,
                "golden": g.golden,
                "matched": g.matched,
                "reconvention": g.reconvention,
                "missing": g.missing,
                "extra": g.extra,
                "whitelisted_missing": g.whitelisted_missing,
                "whitelisted_extra": g.whitelisted_extra,
                "match_pct": g.match_pct_opt(),
            })
        };
        let grps = |v: &[GroupStat]| v.iter().map(grp).collect::<Vec<_>>();
        let diffs = |v: &[DiffItem]| {
            v.iter()
                .map(|d| {
                    json!({
                        "item": d.description,
                        "whitelisted": d.whitelisted,
                        "justification": d.justification,
                    })
                })
                .collect::<Vec<_>>()
        };
        json!({
            "nodes": {
                "total": grp(&self.node_total),
                "match_pct": self.node_total.match_pct_opt(),
                "by_kind": grps(&self.nodes_by_kind),
                "by_language": grps(&self.nodes_by_language),
            },
            "edges": {
                "total": grp(&self.edge_total),
                "calls_match_pct": self.calls_match_pct(),
                "calls_match_pct_raw": self.calls_pct_raw,
                "by_kind": grps(&self.edges_by_kind),
                "by_language": grps(&self.edges_by_language),
            },
            "diffs": {
                "missing_nodes": diffs(&self.missing_nodes),
                "extra_nodes": diffs(&self.extra_nodes),
                "reconvention_nodes": diffs(&self.reconvention_nodes),
                "missing_edges": diffs(&self.missing_edges),
                "extra_edges": diffs(&self.extra_edges),
                "attribute_deltas": diffs(&self.attr_deltas),
            },
        })
    }
}

/// Group nodes by a key into instance lists.
fn group_by<K, F>(nodes: &[PNode], key: F) -> HashMap<K, Vec<&PNode>>
where
    K: std::hash::Hash + Eq,
    F: Fn(&PNode) -> K,
{
    let mut m: HashMap<K, Vec<&PNode>> = HashMap::new();
    for n in nodes {
        m.entry(key(n)).or_default().push(n);
    }
    m
}

/// Result of the node comparison, including the reconvention translation the
/// edge pass needs.
struct NodeCompare {
    total: GroupStat,
    by_kind: Vec<GroupStat>,
    by_language: Vec<GroupStat>,
    missing: Vec<DiffItem>,
    extra: Vec<DiffItem>,
    reconvention: Vec<DiffItem>,
    translation: Translation,
}

fn compare_nodes(ours: &Graph, golden: &Graph, wl: &Whitelist, strict: bool) -> NodeCompare {
    let mut total = GroupStat {
        label: "nodes".into(),
        ..Default::default()
    };
    let mut by_kind: HashMap<String, GroupStat> = HashMap::new();
    let mut by_lang: HashMap<String, GroupStat> = HashMap::new();
    // Bump golden totals up front for every golden node.
    for nd in &golden.nodes {
        total.golden += 1;
        let ks = by_kind.entry(nd.kind.clone()).or_default();
        ks.label = nd.kind.clone();
        ks.golden += 1;
        let ls = by_lang.entry(nd.language.clone()).or_default();
        ls.label = nd.language.clone();
        ls.golden += 1;
    }

    // ---- Exact pass -----------------------------------------------------
    let g_by = group_by(&golden.nodes, |n| n.exact_key(strict));
    let o_by = group_by(&ours.nodes, |n| n.exact_key(strict));
    let mut residual_golden: Vec<&PNode> = Vec::new();
    let mut residual_ours: Vec<&PNode> = Vec::new();

    let mut all_keys: Vec<&NodeKey> = g_by.keys().chain(o_by.keys()).collect();
    all_keys.sort();
    all_keys.dedup();
    for key in all_keys {
        let g = g_by.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let o = o_by.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let matched = g.len().min(o.len());
        if matched > 0 {
            let kind = &g.first().or(o.first()).unwrap().kind;
            let lang = &g.first().or(o.first()).unwrap().language;
            total.matched += matched as i64;
            by_kind.get_mut(kind).unwrap().matched += matched as i64;
            by_lang.get_mut(lang).unwrap().matched += matched as i64;
        }
        residual_golden.extend(g.iter().skip(matched).copied());
        residual_ours.extend(o.iter().skip(matched).copied());
    }

    // ---- Reconvention pass (identity, ignoring qn) ----------------------
    let mut translation: Translation = HashMap::new();
    let mut reconvention = Vec::new();
    let g_res_by = group_by_refs(&residual_golden, |n| n.identity());
    let mut o_res_by = group_by_refs(&residual_ours, |n| n.identity());
    let mut still_missing: Vec<&PNode> = Vec::new();

    let mut gkeys: Vec<&Identity> = g_res_by.keys().collect();
    gkeys.sort();
    for id in gkeys {
        let gs = &g_res_by[id];
        let os = o_res_by.get_mut(id);
        let mut oi = 0usize;
        let opool: &[&PNode] = os.as_deref().map(Vec::as_slice).unwrap_or(&[]);
        for g in gs {
            if oi < opool.len() {
                let o = opool[oi];
                oi += 1;
                // Same physical node, different qn convention.
                total.reconvention += 1;
                by_kind.get_mut(&g.kind).unwrap().reconvention += 1;
                by_lang.get_mut(&g.language).unwrap().reconvention += 1;
                translation.insert(
                    (
                        g.kind.clone(),
                        g.file_path.clone(),
                        g.qualified_name.clone(),
                    ),
                    o.qualified_name.clone(),
                );
                reconvention.push(DiffItem {
                    description: format!(
                        "{} @ {} [{}-{}]: codegraph qn={:?} semgraph qn={:?}",
                        g.kind,
                        g.file_path,
                        g.start_line,
                        g.end_line,
                        g.qualified_name,
                        o.qualified_name
                    ),
                    whitelisted: false,
                    justification: None,
                });
            } else {
                still_missing.push(g);
            }
        }
        // Consume the paired `ours` residuals; the rest stay as extras.
        if let Some(os) = os {
            os.drain(0..oi.min(os.len()));
        }
    }
    let still_extra: Vec<&PNode> = o_res_by.into_values().flatten().collect();

    // ---- Genuine missing / extra + whitelist ----------------------------
    let mut missing = Vec::new();
    for g in still_missing {
        let just = wl.node_justification("missing", g);
        let is_wl = just.is_some();
        total.missing += 1;
        by_kind.get_mut(&g.kind).unwrap().missing += 1;
        by_lang.get_mut(&g.language).unwrap().missing += 1;
        if is_wl {
            total.whitelisted_missing += 1;
            by_kind.get_mut(&g.kind).unwrap().whitelisted_missing += 1;
            by_lang.get_mut(&g.language).unwrap().whitelisted_missing += 1;
        }
        missing.push(DiffItem {
            description: format!(
                "{} {} @ {} [{}-{}]",
                g.kind, g.qualified_name, g.file_path, g.start_line, g.end_line
            ),
            whitelisted: is_wl,
            justification: just.map(str::to_string),
        });
    }
    let mut extra = Vec::new();
    for o in still_extra {
        let just = wl.node_justification("extra", o);
        let is_wl = just.is_some();
        total.extra += 1;
        by_kind.entry(o.kind.clone()).or_default().label = o.kind.clone();
        by_kind.get_mut(&o.kind).unwrap().extra += 1;
        by_lang.entry(o.language.clone()).or_default().label = o.language.clone();
        by_lang.get_mut(&o.language).unwrap().extra += 1;
        if is_wl {
            total.whitelisted_extra += 1;
            by_kind.get_mut(&o.kind).unwrap().whitelisted_extra += 1;
            by_lang.get_mut(&o.language).unwrap().whitelisted_extra += 1;
        }
        extra.push(DiffItem {
            description: format!(
                "{} {} @ {} [{}-{}]",
                o.kind, o.qualified_name, o.file_path, o.start_line, o.end_line
            ),
            whitelisted: is_wl,
            justification: just.map(str::to_string),
        });
    }

    NodeCompare {
        total,
        by_kind: sorted_groups(by_kind),
        by_language: sorted_groups(by_lang),
        missing: sort_diffs(missing),
        extra: sort_diffs(extra),
        reconvention: sort_diffs(reconvention),
        translation,
    }
}

fn group_by_refs<'a, K, F>(nodes: &[&'a PNode], key: F) -> HashMap<K, Vec<&'a PNode>>
where
    K: std::hash::Hash + Eq,
    F: Fn(&PNode) -> K,
{
    let mut m: HashMap<K, Vec<&'a PNode>> = HashMap::new();
    for n in nodes {
        m.entry(key(n)).or_default().push(n);
    }
    m
}

/// Apply the reconvention translation to a golden edge's endpoints, returning
/// the convention-matched `(source_qn, target_qn, kind)` key.
fn translated_key(e: &PEdge, tr: &Translation) -> EdgeKey {
    let src = tr
        .get(&(
            e.source_kind.clone(),
            e.source_file.clone(),
            e.source_qn.clone(),
        ))
        .cloned()
        .unwrap_or_else(|| e.source_qn.clone());
    let tgt = tr
        .get(&(
            e.target_kind.clone(),
            e.target_file.clone(),
            e.target_qn.clone(),
        ))
        .cloned()
        .unwrap_or_else(|| e.target_qn.clone());
    (src, tgt, e.kind.clone())
}

fn raw_key(e: &PEdge) -> EdgeKey {
    (e.source_qn.clone(), e.target_qn.clone(), e.kind.clone())
}

/// Multiset of edge keys → (count, representative edge).
fn edge_multiset<F: Fn(&PEdge) -> EdgeKey>(
    edges: &[PEdge],
    key: F,
) -> HashMap<EdgeKey, (i64, PEdge)> {
    let mut m: HashMap<EdgeKey, (i64, PEdge)> = HashMap::new();
    for e in edges {
        m.entry(key(e)).or_insert((0, e.clone())).0 += 1;
    }
    m
}

struct EdgeCompare {
    total: GroupStat,
    by_kind: Vec<GroupStat>,
    by_language: Vec<GroupStat>,
    missing: Vec<DiffItem>,
    extra: Vec<DiffItem>,
}

/// Compare edges on the **convention-matched** key (golden endpoints translated
/// through the reconvention map).
fn compare_edges(ours: &Graph, golden: &Graph, wl: &Whitelist, tr: &Translation) -> EdgeCompare {
    let our_ek = edge_multiset(&ours.edges, raw_key);
    let golden_ek = edge_multiset(&golden.edges, |e| translated_key(e, tr));

    let mut total = GroupStat {
        label: "edges".into(),
        ..Default::default()
    };
    let mut by_kind: HashMap<String, GroupStat> = HashMap::new();
    let mut by_lang: HashMap<String, GroupStat> = HashMap::new();
    let mut missing = Vec::new();
    let mut extra = Vec::new();

    let mut all: Vec<&EdgeKey> = golden_ek.keys().chain(our_ek.keys()).collect();
    all.sort();
    all.dedup();

    for key in all {
        let (gc, gsample) = golden_ek.get(key).cloned().unwrap_or((0, dummy_edge()));
        let (oc, osample) = our_ek.get(key).cloned().unwrap_or((0, dummy_edge()));
        let sample = if gc > 0 { &gsample } else { &osample };
        let matched = gc.min(oc);
        let miss = (gc - oc).max(0);
        let ext = (oc - gc).max(0);

        let ks = by_kind.entry(sample.kind.clone()).or_default();
        ks.label = sample.kind.clone();
        let ls = by_lang.entry(sample.language.clone()).or_default();
        ls.label = sample.language.clone();
        for stat in [&mut total, &mut *ks, &mut *ls] {
            stat.golden += gc;
            stat.matched += matched;
            stat.missing += miss;
            stat.extra += ext;
        }

        if miss > 0 {
            let just = wl.edge_justification("missing", key, oc);
            let is_wl = just.is_some();
            if is_wl {
                for stat in [&mut total, &mut *ks, &mut *ls] {
                    stat.whitelisted_missing += miss;
                }
            }
            missing.push(DiffItem {
                description: format!(
                    "{}x {} {} -> {}",
                    miss, gsample.kind, gsample.source_qn, gsample.target_qn
                ),
                whitelisted: is_wl,
                justification: just.map(str::to_string),
            });
        }
        if ext > 0 {
            let just = wl.edge_justification("extra", key, oc);
            let is_wl = just.is_some();
            if is_wl {
                for stat in [&mut total, &mut *ks, &mut *ls] {
                    stat.whitelisted_extra += ext;
                }
            }
            extra.push(DiffItem {
                description: format!(
                    "{}x {} {} -> {}",
                    ext, osample.kind, osample.source_qn, osample.target_qn
                ),
                whitelisted: is_wl,
                justification: just.map(str::to_string),
            });
        }
    }

    EdgeCompare {
        total,
        by_kind: sorted_groups(by_kind),
        by_language: sorted_groups(by_lang),
        missing: sort_diffs(missing),
        extra: sort_diffs(extra),
    }
}

/// Raw (untranslated) `calls` recall — how the calls parity looks *before*
/// normalizing the qn convention. `None` when golden has no `calls` edges.
fn raw_calls_pct(ours: &Graph, golden: &Graph) -> Option<f64> {
    let our_ek = edge_multiset(&ours.edges, raw_key);
    let golden_ek = edge_multiset(&golden.edges, raw_key);
    let mut golden_calls = 0i64;
    let mut matched = 0i64;
    for (key, (gc, _)) in &golden_ek {
        if key.2 != "calls" {
            continue;
        }
        golden_calls += gc;
        let oc = our_ek.get(key).map(|(c, _)| *c).unwrap_or(0);
        matched += (*gc).min(oc);
    }
    (golden_calls > 0).then(|| 100.0 * matched as f64 / golden_calls as f64)
}

/// Compare a semgraph graph (`ours`) against a CodeGraph golden graph
/// (`golden`), applying `whitelist`.
pub fn compare(
    ours: &Graph,
    golden: &Graph,
    whitelist: &Whitelist,
    opts: &CompareOptions,
) -> ParityReport {
    let nodes = compare_nodes(ours, golden, whitelist, opts.strict_line_range);
    let attr_deltas = attribute_deltas(ours, golden, whitelist, opts.strict_line_range);
    let edges = compare_edges(ours, golden, whitelist, &nodes.translation);
    let calls_pct_raw = raw_calls_pct(ours, golden);

    ParityReport {
        node_total: nodes.total,
        edge_total: edges.total,
        nodes_by_kind: nodes.by_kind,
        nodes_by_language: nodes.by_language,
        edges_by_kind: edges.by_kind,
        edges_by_language: edges.by_language,
        missing_nodes: nodes.missing,
        extra_nodes: nodes.extra,
        reconvention_nodes: nodes.reconvention,
        missing_edges: edges.missing,
        extra_edges: edges.extra,
        attr_deltas,
        calls_pct_raw,
    }
}

/// Compare attributes of nodes matched on physical identity. Reports `is_async`,
/// `docstring`, `signature`, and (when not strict) `line_range` differences.
fn attribute_deltas(
    ours: &Graph,
    golden: &Graph,
    whitelist: &Whitelist,
    strict: bool,
) -> Vec<DiffItem> {
    // Pair on physical identity so convention differences in qn don't prevent
    // attribute comparison of the same node.
    let our_by = group_by(&ours.nodes, |n| n.identity());
    let golden_by = group_by(&golden.nodes, |n| n.identity());

    let mut out = Vec::new();
    let mut keys: Vec<&Identity> = golden_by.keys().collect();
    keys.sort();
    for key in keys {
        let (Some(gs), Some(os)) = (golden_by.get(key), our_by.get(key)) else {
            continue;
        };
        for (g, o) in gs.iter().zip(os.iter()) {
            let mut checks: Vec<(&str, String, String)> = vec![
                ("is_async", g.is_async.to_string(), o.is_async.to_string()),
                ("docstring", opt_str(&g.docstring), opt_str(&o.docstring)),
                ("signature", opt_str(&g.signature), opt_str(&o.signature)),
            ];
            if !strict {
                checks.push((
                    "line_range",
                    format!("{}-{}", g.start_line, g.end_line),
                    format!("{}-{}", o.start_line, o.end_line),
                ));
            }
            for (attr, gv, ov) in checks {
                if gv == ov {
                    continue;
                }
                let just = whitelist.node_attr_justification(attr, o, &ov, &gv);
                out.push(DiffItem {
                    description: format!(
                        "{} {} @ {} [{}]: codegraph={:?} semgraph={:?}",
                        o.kind, o.qualified_name, o.file_path, attr, gv, ov
                    ),
                    whitelisted: just.is_some(),
                    justification: just.map(str::to_string),
                });
            }
        }
    }
    sort_diffs(out)
}

fn opt_str(o: &Option<String>) -> String {
    o.clone().unwrap_or_default()
}

fn sorted_groups(m: HashMap<String, GroupStat>) -> Vec<GroupStat> {
    let mut v: Vec<GroupStat> = m.into_values().collect();
    v.sort_by(|a, b| a.label.cmp(&b.label));
    v
}

fn sort_diffs(mut v: Vec<DiffItem>) -> Vec<DiffItem> {
    v.sort_by(|a, b| {
        a.whitelisted
            .cmp(&b.whitelisted)
            .then_with(|| a.description.cmp(&b.description))
    });
    v
}

fn dummy_edge() -> PEdge {
    PEdge {
        source_qn: String::new(),
        target_qn: String::new(),
        kind: String::new(),
        language: String::new(),
        source_kind: String::new(),
        source_file: String::new(),
        target_kind: String::new(),
        target_file: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(kind: &str, qn: &str, file: &str, lang: &str) -> PNode {
        PNode {
            kind: kind.into(),
            qualified_name: qn.into(),
            file_path: file.into(),
            language: lang.into(),
            start_line: 1,
            end_line: 2,
            is_async: false,
            docstring: None,
            signature: None,
        }
    }
    fn nl(kind: &str, qn: &str, file: &str, lang: &str, s: i64, e: i64) -> PNode {
        PNode {
            start_line: s,
            end_line: e,
            ..n(kind, qn, file, lang)
        }
    }
    fn e(src: &str, tgt: &str, kind: &str, lang: &str) -> PEdge {
        PEdge {
            source_qn: src.into(),
            target_qn: tgt.into(),
            kind: kind.into(),
            language: lang.into(),
            source_kind: "function".into(),
            source_file: "a.rs".into(),
            target_kind: "function".into(),
            target_file: "a.rs".into(),
        }
    }

    #[test]
    fn glob_matches_star_prefix_suffix_infix() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("Array<*>", "Array<Scalar>"));
        assert!(glob_match("python/*", "python/main.py"));
        assert!(glob_match("*.py", "a/b/main.py"));
        assert!(!glob_match("python/*", "rust/lib.rs"));
        assert!(glob_match("exact", "exact"));
    }

    #[test]
    fn identical_graphs_are_100_percent() {
        let g = Graph {
            nodes: vec![n("function", "f", "a.rs", "rust")],
            edges: vec![e("f", "g", "calls", "rust")],
        };
        let r = compare(&g, &g, &Whitelist::default(), &CompareOptions::default());
        assert_eq!(r.node_match_pct(), 100.0);
        assert_eq!(r.calls_match_pct(), Some(100.0));
        assert!(r.passes(95.0, 90.0));
    }

    #[test]
    fn missing_node_lowers_recall() {
        let golden = Graph {
            nodes: vec![
                nl("function", "f", "a.rs", "rust", 1, 2),
                nl("function", "g", "a.rs", "rust", 5, 6),
            ],
            edges: vec![],
        };
        let ours = Graph {
            nodes: vec![nl("function", "f", "a.rs", "rust", 1, 2)],
            edges: vec![],
        };
        let r = compare(
            &ours,
            &golden,
            &Whitelist::default(),
            &CompareOptions::default(),
        );
        assert_eq!(r.node_match_pct(), 50.0);
        assert_eq!(r.missing_nodes.iter().filter(|d| !d.whitelisted).count(), 1);
    }

    #[test]
    fn reconvention_credits_recall_and_translates_edges() {
        // Same physical node (function @ a.rs [10-20]) named `tests::foo` by
        // CodeGraph and `foo` by semgraph. And a calls edge into it.
        let golden = Graph {
            nodes: vec![
                nl("function", "caller", "a.rs", "rust", 1, 3),
                nl("function", "tests::foo", "a.rs", "rust", 10, 20),
            ],
            edges: vec![PEdge {
                source_qn: "caller".into(),
                target_qn: "tests::foo".into(),
                kind: "calls".into(),
                language: "rust".into(),
                source_kind: "function".into(),
                source_file: "a.rs".into(),
                target_kind: "function".into(),
                target_file: "a.rs".into(),
            }],
        };
        let ours = Graph {
            nodes: vec![
                nl("function", "caller", "a.rs", "rust", 1, 3),
                nl("function", "foo", "a.rs", "rust", 10, 20),
            ],
            edges: vec![PEdge {
                source_qn: "caller".into(),
                target_qn: "foo".into(),
                kind: "calls".into(),
                language: "rust".into(),
                source_kind: "function".into(),
                source_file: "a.rs".into(),
                target_kind: "function".into(),
                target_file: "a.rs".into(),
            }],
        };
        let r = compare(
            &ours,
            &golden,
            &Whitelist::default(),
            &CompareOptions::default(),
        );
        // Node: 1 exact (caller) + 1 reconvention (foo) => 100% recall, but the
        // reconvention is NOT counted as a genuine miss.
        assert_eq!(r.node_match_pct(), 100.0);
        assert_eq!(r.node_total.reconvention, 1);
        assert_eq!(r.missing_nodes.iter().filter(|d| !d.whitelisted).count(), 0);
        assert_eq!(r.reconvention_nodes.len(), 1);
        // Edge: convention-matched calls parity is 100%; the RAW qn key would
        // have missed it (tests::foo != foo).
        assert_eq!(r.calls_match_pct(), Some(100.0));
        assert_eq!(r.calls_pct_raw, Some(0.0));
    }

    #[test]
    fn duplicate_reference_only_whitelisted_when_key_still_emitted() {
        let golden = Graph {
            nodes: vec![],
            edges: vec![
                e("a", "b", "references", "typescript"),
                e("a", "b", "references", "typescript"),
            ],
        };
        let ours = Graph {
            nodes: vec![],
            edges: vec![e("a", "b", "references", "typescript")],
        };
        let wl = Whitelist::from_json(
            r#"{"edges":[{"side":"missing","kind":"references","only_duplicates":true,"justification":"dup"}]}"#,
        )
        .unwrap();
        let r = compare(&ours, &golden, &wl, &CompareOptions::default());
        let refs = r
            .edges_by_kind
            .iter()
            .find(|g| g.label == "references")
            .unwrap();
        assert_eq!(refs.missing, 1);
        assert_eq!(refs.whitelisted_missing, 1);
        assert_eq!(refs.match_pct(), 100.0);

        // A true recall gap (ours emits zero) is NOT whitelisted.
        let r2 = compare(&Graph::default(), &golden, &wl, &CompareOptions::default());
        let refs2 = r2
            .edges_by_kind
            .iter()
            .find(|g| g.label == "references")
            .unwrap();
        assert_eq!(refs2.missing, 2);
        assert_eq!(refs2.whitelisted_missing, 0);
    }

    #[test]
    fn is_async_whitelist_is_direction_specific() {
        // Documented-better: semgraph=true, codegraph=false → forgiven.
        let mut better = n("function", "gather", "m.py", "python");
        better.is_async = true;
        let golden = Graph {
            nodes: vec![n("function", "gather", "m.py", "python")],
            edges: vec![],
        };
        let ours = Graph {
            nodes: vec![better],
            edges: vec![],
        };
        let wl = Whitelist::from_json(
            r#"{"node_attrs":[{"attr":"is_async","direction":"semgraph_true","justification":"ADR-003"}]}"#,
        )
        .unwrap();
        let r = compare(&ours, &golden, &wl, &CompareOptions::default());
        let d = r
            .attr_deltas
            .iter()
            .find(|d| d.description.contains("is_async"))
            .unwrap();
        assert!(d.whitelisted, "better direction must be forgiven");

        // REGRESSION: semgraph=false, codegraph=true → must NOT be forgiven.
        let mut cg_async = n("function", "gather", "m.py", "python");
        cg_async.is_async = true;
        let golden2 = Graph {
            nodes: vec![cg_async],
            edges: vec![],
        };
        let ours2 = Graph {
            nodes: vec![n("function", "gather", "m.py", "python")],
            edges: vec![],
        };
        let r2 = compare(&ours2, &golden2, &wl, &CompareOptions::default());
        let d2 = r2
            .attr_deltas
            .iter()
            .find(|d| d.description.contains("is_async"))
            .unwrap();
        assert!(
            !d2.whitelisted,
            "a lost is_async is a regression, not forgiven"
        );
    }

    #[test]
    fn docstring_whitelist_does_not_mask_a_dropped_docstring() {
        let wl = Whitelist::from_json(
            r#"{"node_attrs":[{"attr":"docstring","direction":"semgraph_nonempty","justification":"ADR-003"}]}"#,
        )
        .unwrap();
        // Better: semgraph has a docstring, codegraph empty → forgiven.
        let mut o = n("function", "f", "a.rs", "rust");
        o.docstring = Some("clean".into());
        let r = compare(
            &Graph {
                nodes: vec![o],
                edges: vec![],
            },
            &Graph {
                nodes: vec![n("function", "f", "a.rs", "rust")],
                edges: vec![],
            },
            &wl,
            &CompareOptions::default(),
        );
        assert!(
            r.attr_deltas
                .iter()
                .find(|d| d.description.contains("docstring"))
                .unwrap()
                .whitelisted
        );

        // Regression: semgraph empty, codegraph had one → NOT forgiven.
        let mut g = n("function", "f", "a.rs", "rust");
        g.docstring = Some("real".into());
        let r2 = compare(
            &Graph {
                nodes: vec![n("function", "f", "a.rs", "rust")],
                edges: vec![],
            },
            &Graph {
                nodes: vec![g],
                edges: vec![],
            },
            &wl,
            &CompareOptions::default(),
        );
        assert!(
            !r2.attr_deltas
                .iter()
                .find(|d| d.description.contains("docstring"))
                .unwrap()
                .whitelisted
        );
    }

    #[test]
    fn zero_calls_reports_na_and_gate_is_vacuous() {
        // A graph with nodes but no calls edges: calls parity is N/A, not 100%.
        let g = Graph {
            nodes: vec![n("function", "f", "a.rs", "rust")],
            edges: vec![],
        };
        let r = compare(&g, &g, &Whitelist::default(), &CompareOptions::default());
        assert_eq!(r.calls_match_pct(), None);
        assert!(
            r.passes(95.0, 90.0),
            "no calls => calls gate vacuously satisfied"
        );
    }

    #[test]
    fn calls_recall_gates_independently_of_nodes() {
        let golden = Graph {
            nodes: vec![],
            edges: vec![e("f", "g", "calls", "rust"), e("f", "h", "calls", "rust")],
        };
        let ours = Graph {
            nodes: vec![],
            edges: vec![e("f", "g", "calls", "rust")],
        };
        let r = compare(
            &ours,
            &golden,
            &Whitelist::default(),
            &CompareOptions::default(),
        );
        assert_eq!(r.calls_match_pct(), Some(50.0));
        assert!(!r.passes(0.0, 90.0));
    }
}
