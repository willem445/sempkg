//! Parity harness core: quantify how closely a **semgraph**-built graph matches
//! a **CodeGraph 0.9.7**-built one (issue #78, Phase 2 item 6 / "P2c").
//!
//! This module is the pure, testable engine behind the `parity` binary
//! (`src/bin/parity.rs`). It is deliberately self-contained — it reads two
//! schema-v4 `codegraph.db` files, diffs them, applies a committed *whitelist*
//! of known-better deviations (ADR-003/004), and produces a machine-readable
//! [`ParityReport`] with per-kind / per-language match percentages plus
//! missing/extra listings. The binary layers CLI parsing, the semgraph index
//! step, and the optional live CodeGraph shell-out on top.
//!
//! ## What is compared
//!
//! - **Nodes** are keyed on `(kind, qualified_name, file_path)` and, when
//!   [`CompareOptions::strict_line_range`] is set, additionally on
//!   `(start_line, end_line)`. Line-range is otherwise compared as a node
//!   *attribute* (reported, whitelistable) rather than as a hard match key, so a
//!   one-line drift on a real repo does not double-count as both a missing and an
//!   extra node. The issue lists line-range among the match dimensions; this
//!   keeps it in the diff without letting it dominate the headline percentage.
//! - **Edges** are keyed on `(source_qn, target_qn, kind)` as a **multiset**, so
//!   duplicate call sites (`Point::new` twice) and CodeGraph's duplicate
//!   return-type references are both represented faithfully.
//! - **Node attributes** (`is_async`, `docstring`, `signature`, and, in the
//!   default relaxed mode, the line-range) are compared for every node matched on
//!   the relaxed key. Divergences are reported and can be whitelisted.
//!
//! ## Known-better deltas (the whitelist)
//!
//! ADR-003 and ADR-004 record deliberate improvements over CodeGraph 0.9.7 that
//! must **not** count as parity failures: `is_async` correctness across all
//! languages, docstring cleanups, and the omission of CodeGraph's duplicate
//! return-type `references`. These are expressed in a committed
//! [`Whitelist`] file with a per-entry justification; whitelisted diffs are
//! counted and reported *separately*, never as failures.

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
    /// The relaxed match key: `(kind, qualified_name, file_path)`.
    fn relaxed_key(&self) -> NodeKey {
        (
            self.kind.clone(),
            self.qualified_name.clone(),
            self.file_path.clone(),
            None,
        )
    }
    /// The strict match key, additionally pinning the line range.
    fn strict_key(&self) -> NodeKey {
        (
            self.kind.clone(),
            self.qualified_name.clone(),
            self.file_path.clone(),
            Some((self.start_line, self.end_line)),
        )
    }
    fn key(&self, strict: bool) -> NodeKey {
        if strict {
            self.strict_key()
        } else {
            self.relaxed_key()
        }
    }
}

type NodeKey = (String, String, String, Option<(i64, i64)>);

/// An edge projected for parity comparison. Language is the **caller's**
/// (source node's) language, matching ADR-004's language-scoped resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PEdge {
    pub source_qn: String,
    pub target_qn: String,
    pub kind: String,
    pub language: String,
}

impl PEdge {
    fn key(&self) -> EdgeKey {
        (
            self.source_qn.clone(),
            self.target_qn.clone(),
            self.kind.clone(),
        )
    }
}

type EdgeKey = (String, String, String);

/// The two projections read out of one `codegraph.db`.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: Vec<PNode>,
    pub edges: Vec<PEdge>,
}

/// Read the parity projection (nodes + edges) out of a schema-v4 `codegraph.db`.
///
/// Works on any such database, whether produced by CodeGraph or by
/// [`crate::index_roots`]. Opens read-write-less (the default) since callers may
/// pass a freshly-written temp DB.
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
        "SELECT s.qualified_name, t.qualified_name, e.kind, COALESCE(s.language,'') \
         FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target",
    )?;
    let edges = estmt
        .query_map([], |r| {
            Ok(PEdge {
                source_qn: r.get(0)?,
                target_qn: r.get(1)?,
                kind: r.get(2)?,
                language: r.get(3)?,
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
    pub justification: String,
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

    fn node_attr_justification(&self, attr: &str, n: &PNode) -> Option<&str> {
        self.node_attrs.iter().find_map(|r| {
            (r.attr == attr
                && glob_match(&r.qualified_name, &n.qualified_name)
                && glob_match(&r.file, &n.file_path)
                && r.language.as_deref().is_none_or(|l| l == n.language))
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
/// literal. `"*"` alone matches anything. Sufficient for whitelist patterns like
/// `Array<*>` or `python/*`.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    // No '*' → exact match.
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // Anchored prefix.
            if !text[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            // Anchored suffix.
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
    /// Pin `(start_line, end_line)` into the node match key. Off by default; when
    /// off, line-range is compared as a whitelistable attribute instead.
    pub strict_line_range: bool,
}

/// Per-`kind` (or per-language) match counts for one entity type.
#[derive(Debug, Clone, Default)]
pub struct GroupStat {
    pub label: String,
    pub golden: i64,
    pub matched: i64,
    pub missing: i64,
    pub extra: i64,
    pub whitelisted_missing: i64,
    pub whitelisted_extra: i64,
}

impl GroupStat {
    /// Recall percentage after crediting whitelisted omissions. 100% when the
    /// golden side is empty.
    pub fn match_pct(&self) -> f64 {
        if self.golden == 0 {
            return 100.0;
        }
        let effective_missing = (self.missing - self.whitelisted_missing).max(0);
        100.0 * (self.golden - effective_missing) as f64 / self.golden as f64
    }
}

/// A single reported diff item (missing/extra node or edge, or an attribute
/// delta), with its whitelist justification when one applies.
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
    pub missing_edges: Vec<DiffItem>,
    pub extra_edges: Vec<DiffItem>,
    pub attr_deltas: Vec<DiffItem>,
}

impl ParityReport {
    /// Overall node recall after whitelist (the `--min-nodes` metric).
    pub fn node_match_pct(&self) -> f64 {
        self.node_total.match_pct()
    }

    /// `calls`-edge recall after whitelist (the `--min-calls` metric).
    pub fn calls_match_pct(&self) -> f64 {
        self.edges_by_kind
            .iter()
            .find(|g| g.label == "calls")
            .map(GroupStat::match_pct)
            .unwrap_or(100.0)
    }

    /// Whether the report clears both acceptance thresholds.
    pub fn passes(&self, min_nodes: f64, min_calls: f64) -> bool {
        self.node_match_pct() >= min_nodes && self.calls_match_pct() >= min_calls
    }

    /// Machine-readable summary.
    pub fn to_json(&self) -> Value {
        let grp = |g: &GroupStat| {
            json!({
                "label": g.label,
                "golden": g.golden,
                "matched": g.matched,
                "missing": g.missing,
                "extra": g.extra,
                "whitelisted_missing": g.whitelisted_missing,
                "whitelisted_extra": g.whitelisted_extra,
                "match_pct": round2(g.match_pct()),
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
                "match_pct": round2(self.node_match_pct()),
                "by_kind": grps(&self.nodes_by_kind),
                "by_language": grps(&self.nodes_by_language),
            },
            "edges": {
                "total": grp(&self.edge_total),
                "calls_match_pct": round2(self.calls_match_pct()),
                "by_kind": grps(&self.edges_by_kind),
                "by_language": grps(&self.edges_by_language),
            },
            "diffs": {
                "missing_nodes": diffs(&self.missing_nodes),
                "extra_nodes": diffs(&self.extra_nodes),
                "missing_edges": diffs(&self.missing_edges),
                "extra_edges": diffs(&self.extra_edges),
                "attribute_deltas": diffs(&self.attr_deltas),
            },
        })
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Multiset of keys → (count, one representative value).
fn multiset<K: std::hash::Hash + Eq + Clone, V: Clone>(
    items: impl IntoIterator<Item = (K, V)>,
) -> HashMap<K, (i64, V)> {
    let mut m: HashMap<K, (i64, V)> = HashMap::new();
    for (k, v) in items {
        m.entry(k).or_insert((0, v)).0 += 1;
    }
    m
}

/// Compare a semgraph graph (`ours`) against a CodeGraph golden graph
/// (`golden`), applying `whitelist`.
pub fn compare(
    ours: &Graph,
    golden: &Graph,
    whitelist: &Whitelist,
    opts: &CompareOptions,
) -> ParityReport {
    let strict = opts.strict_line_range;

    // ---- Nodes -----------------------------------------------------------
    let our_nodes = multiset(ours.nodes.iter().map(|n| (n.key(strict), n.clone())));
    let golden_nodes = multiset(golden.nodes.iter().map(|n| (n.key(strict), n.clone())));

    let mut node_total = GroupStat {
        label: "nodes".into(),
        ..Default::default()
    };
    let mut by_kind: HashMap<String, GroupStat> = HashMap::new();
    let mut by_lang: HashMap<String, GroupStat> = HashMap::new();
    let mut missing_nodes = Vec::new();
    let mut extra_nodes = Vec::new();

    // Union of keys.
    let mut all_keys: Vec<&NodeKey> = golden_nodes.keys().chain(our_nodes.keys()).collect();
    all_keys.sort();
    all_keys.dedup();

    for key in all_keys {
        let (gc, gsample) = golden_nodes.get(key).cloned().unwrap_or((0, dummy_node()));
        let (oc, osample) = our_nodes.get(key).cloned().unwrap_or((0, dummy_node()));
        let sample = if gc > 0 { &gsample } else { &osample };
        let matched = gc.min(oc);
        let missing = (gc - oc).max(0);
        let extra = (oc - gc).max(0);

        let ks = by_kind.entry(sample.kind.clone()).or_default();
        ks.label = sample.kind.clone();
        let ls = by_lang.entry(sample.language.clone()).or_default();
        ls.label = sample.language.clone();

        for stat in [&mut node_total, &mut *ks, &mut *ls] {
            stat.golden += gc;
            stat.matched += matched;
            stat.missing += missing;
            stat.extra += extra;
        }

        if missing > 0 {
            let just = whitelist.node_justification("missing", &gsample);
            let wl = just.is_some();
            if wl {
                for stat in [&mut node_total, &mut *ks, &mut *ls] {
                    stat.whitelisted_missing += missing;
                }
            }
            missing_nodes.push(DiffItem {
                description: format!(
                    "{}x {} {} @ {} [{}-{}]",
                    missing,
                    gsample.kind,
                    gsample.qualified_name,
                    gsample.file_path,
                    gsample.start_line,
                    gsample.end_line
                ),
                whitelisted: wl,
                justification: just.map(str::to_string),
            });
        }
        if extra > 0 {
            let just = whitelist.node_justification("extra", &osample);
            let wl = just.is_some();
            if wl {
                for stat in [&mut node_total, &mut *ks, &mut *ls] {
                    stat.whitelisted_extra += extra;
                }
            }
            extra_nodes.push(DiffItem {
                description: format!(
                    "{}x {} {} @ {} [{}-{}]",
                    extra,
                    osample.kind,
                    osample.qualified_name,
                    osample.file_path,
                    osample.start_line,
                    osample.end_line
                ),
                whitelisted: wl,
                justification: just.map(str::to_string),
            });
        }
    }

    // ---- Node attribute deltas (relaxed pairing) -------------------------
    let attr_deltas = attribute_deltas(ours, golden, whitelist, strict);

    // ---- Edges -----------------------------------------------------------
    let our_edges = multiset(ours.edges.iter().map(|e| (e.key(), e.clone())));
    let golden_edges = multiset(golden.edges.iter().map(|e| (e.key(), e.clone())));

    let mut edge_total = GroupStat {
        label: "edges".into(),
        ..Default::default()
    };
    let mut ek_by_kind: HashMap<String, GroupStat> = HashMap::new();
    let mut ek_by_lang: HashMap<String, GroupStat> = HashMap::new();
    let mut missing_edges = Vec::new();
    let mut extra_edges = Vec::new();

    let mut all_ekeys: Vec<&EdgeKey> = golden_edges.keys().chain(our_edges.keys()).collect();
    all_ekeys.sort();
    all_ekeys.dedup();

    for key in all_ekeys {
        let (gc, gsample) = golden_edges.get(key).cloned().unwrap_or((0, dummy_edge()));
        let (oc, osample) = our_edges.get(key).cloned().unwrap_or((0, dummy_edge()));
        let sample = if gc > 0 { &gsample } else { &osample };
        let matched = gc.min(oc);
        let missing = (gc - oc).max(0);
        let extra = (oc - gc).max(0);

        let ks = ek_by_kind.entry(sample.kind.clone()).or_default();
        ks.label = sample.kind.clone();
        let ls = ek_by_lang.entry(sample.language.clone()).or_default();
        ls.label = sample.language.clone();

        for stat in [&mut edge_total, &mut *ks, &mut *ls] {
            stat.golden += gc;
            stat.matched += matched;
            stat.missing += missing;
            stat.extra += extra;
        }

        if missing > 0 {
            let just = whitelist.edge_justification("missing", key, oc);
            let wl = just.is_some();
            if wl {
                for stat in [&mut edge_total, &mut *ks, &mut *ls] {
                    stat.whitelisted_missing += missing;
                }
            }
            missing_edges.push(DiffItem {
                description: format!(
                    "{}x {} {} -> {}",
                    missing, gsample.kind, gsample.source_qn, gsample.target_qn
                ),
                whitelisted: wl,
                justification: just.map(str::to_string),
            });
        }
        if extra > 0 {
            let just = whitelist.edge_justification("extra", key, oc);
            let wl = just.is_some();
            if wl {
                for stat in [&mut edge_total, &mut *ks, &mut *ls] {
                    stat.whitelisted_extra += extra;
                }
            }
            extra_edges.push(DiffItem {
                description: format!(
                    "{}x {} {} -> {}",
                    extra, osample.kind, osample.source_qn, osample.target_qn
                ),
                whitelisted: wl,
                justification: just.map(str::to_string),
            });
        }
    }

    ParityReport {
        node_total,
        edge_total,
        nodes_by_kind: sorted_groups(by_kind),
        nodes_by_language: sorted_groups(by_lang),
        edges_by_kind: sorted_groups(ek_by_kind),
        edges_by_language: sorted_groups(ek_by_lang),
        missing_nodes: sort_diffs(missing_nodes),
        extra_nodes: sort_diffs(extra_nodes),
        missing_edges: sort_diffs(missing_edges),
        extra_edges: sort_diffs(extra_edges),
        attr_deltas,
    }
}

/// Compare attributes of nodes matched on the relaxed key. Reports `is_async`,
/// `docstring`, `signature`, and (when not strict) `line_range` differences.
fn attribute_deltas(
    ours: &Graph,
    golden: &Graph,
    whitelist: &Whitelist,
    strict: bool,
) -> Vec<DiffItem> {
    let mut our_by: HashMap<NodeKey, Vec<&PNode>> = HashMap::new();
    for n in &ours.nodes {
        our_by.entry(n.relaxed_key()).or_default().push(n);
    }
    let mut golden_by: HashMap<NodeKey, Vec<&PNode>> = HashMap::new();
    for n in &golden.nodes {
        golden_by.entry(n.relaxed_key()).or_default().push(n);
    }

    let mut out = Vec::new();
    let mut keys: Vec<&NodeKey> = golden_by.keys().collect();
    keys.sort();
    for key in keys {
        let (Some(gs), Some(os)) = (golden_by.get(key), our_by.get(key)) else {
            continue;
        };
        // Pair positionally up to the shorter side (duplicate defs are rare).
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
                let just = whitelist.node_attr_justification(attr, o);
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
    // Non-whitelisted first (they matter most), then alphabetical.
    v.sort_by(|a, b| {
        a.whitelisted
            .cmp(&b.whitelisted)
            .then_with(|| a.description.cmp(&b.description))
    });
    v
}

fn dummy_node() -> PNode {
    PNode {
        kind: String::new(),
        qualified_name: String::new(),
        file_path: String::new(),
        language: String::new(),
        start_line: 0,
        end_line: 0,
        is_async: false,
        docstring: None,
        signature: None,
    }
}

fn dummy_edge() -> PEdge {
    PEdge {
        source_qn: String::new(),
        target_qn: String::new(),
        kind: String::new(),
        language: String::new(),
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
    fn e(src: &str, tgt: &str, kind: &str, lang: &str) -> PEdge {
        PEdge {
            source_qn: src.into(),
            target_qn: tgt.into(),
            kind: kind.into(),
            language: lang.into(),
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
        assert!(!glob_match("exact", "other"));
    }

    #[test]
    fn identical_graphs_are_100_percent() {
        let g = Graph {
            nodes: vec![n("function", "f", "a.rs", "rust")],
            edges: vec![e("f", "g", "calls", "rust")],
        };
        let r = compare(&g, &g, &Whitelist::default(), &CompareOptions::default());
        assert_eq!(r.node_match_pct(), 100.0);
        assert_eq!(r.calls_match_pct(), 100.0);
        assert!(r.passes(95.0, 90.0));
    }

    #[test]
    fn missing_node_lowers_recall() {
        let golden = Graph {
            nodes: vec![
                n("function", "f", "a.rs", "rust"),
                n("function", "g", "a.rs", "rust"),
            ],
            edges: vec![],
        };
        let ours = Graph {
            nodes: vec![n("function", "f", "a.rs", "rust")],
            edges: vec![],
        };
        let r = compare(
            &ours,
            &golden,
            &Whitelist::default(),
            &CompareOptions::default(),
        );
        assert_eq!(r.node_match_pct(), 50.0);
        assert_eq!(r.missing_nodes.len(), 1);
        assert!(!r.missing_nodes[0].whitelisted);
    }

    #[test]
    fn whitelisted_missing_node_is_credited_but_reported() {
        let golden = Graph {
            nodes: vec![
                n("function", "f", "a.rs", "rust"),
                n("function", "g", "a.rs", "rust"),
            ],
            edges: vec![],
        };
        let ours = Graph {
            nodes: vec![n("function", "f", "a.rs", "rust")],
            edges: vec![],
        };
        let wl = Whitelist::from_json(
            r#"{"nodes":[{"side":"missing","qualified_name":"g","justification":"known"}]}"#,
        )
        .unwrap();
        let r = compare(&ours, &golden, &wl, &CompareOptions::default());
        // Credited back to 100%, but still listed as a whitelisted diff.
        assert_eq!(r.node_match_pct(), 100.0);
        assert_eq!(r.missing_nodes.len(), 1);
        assert!(r.missing_nodes[0].whitelisted);
        assert_eq!(r.node_total.whitelisted_missing, 1);
    }

    #[test]
    fn duplicate_reference_only_whitelisted_when_key_still_emitted() {
        // golden emits (a->b references) twice; ours once.
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

        // But a *true* recall gap (ours emits zero) is NOT whitelisted by
        // only_duplicates.
        let ours_zero = Graph {
            nodes: vec![],
            edges: vec![],
        };
        let r2 = compare(&ours_zero, &golden, &wl, &CompareOptions::default());
        let refs2 = r2
            .edges_by_kind
            .iter()
            .find(|g| g.label == "references")
            .unwrap();
        assert_eq!(refs2.missing, 2);
        assert_eq!(refs2.whitelisted_missing, 0);
    }

    #[test]
    fn is_async_attribute_delta_is_whitelistable() {
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
            r#"{"node_attrs":[{"attr":"is_async","justification":"ADR-003"}]}"#,
        )
        .unwrap();
        let r = compare(&ours, &golden, &wl, &CompareOptions::default());
        // Node still matches on the key (is_async isn't part of it).
        assert_eq!(r.node_match_pct(), 100.0);
        let d = r
            .attr_deltas
            .iter()
            .find(|d| d.description.contains("is_async"))
            .expect("is_async delta reported");
        assert!(d.whitelisted, "should be whitelisted: {d:?}");
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
        assert_eq!(r.calls_match_pct(), 50.0);
        assert!(!r.passes(0.0, 90.0));
    }
}
