//! Edge-resolution parity vs the golden CodeGraph 0.9.7 fixture (issue #78,
//! Phase 2b).
//!
//! The compatibility contract for resolution: a native `index_roots` of
//! `tests/fixtures/graph-src` must reproduce CodeGraph's **`calls`** edges
//! exactly (source qn, target qn, kind, line, **col**) — the graded metric (≥90%
//! per the issue; we hold 100% on the fixture) — and its `imports`/
//! `instantiates` edges exactly. `references` are reproduced up to a small,
//! documented whitelist of CodeGraph return-type duplicate emissions.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use semgraph::{index_roots, IndexOptions};

fn graph_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/graph-src")
}

fn golden_db() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/codegraph-v4.db")
}

/// An edge as `(source_qn, target_qn, kind, line, col)` — the parity key.
/// `line`/`col` are the call-site coordinates (`edges.line`/`edges.col`).
type EdgeKey = (String, String, String, i64, i64);

/// Read every non-`contains` edge from `db_path`, joined back to node
/// qualified names, keyed by `(source_qn, target_qn, kind, line, col)`.
fn edge_keys(db_path: &Path, kind: &str) -> Vec<EdgeKey> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT s.qualified_name, t.qualified_name, e.kind, \
                    COALESCE(e.line, -1), COALESCE(e.col, -1) \
             FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target \
             WHERE e.kind = ?1",
        )
        .unwrap();
    stmt.query_map([kind], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

fn multiset(keys: Vec<EdgeKey>) -> Vec<EdgeKey> {
    let mut v = keys;
    v.sort();
    v
}

fn build() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("codegraph.db");
    index_roots(&[graph_src()], &db_path, &IndexOptions::default()).unwrap();
    dir
}

/// The golden `calls` edges are reproduced EXACTLY — same multiset of
/// `(source_qn, target_qn, kind, line)`. This is the issue's graded metric.
#[test]
fn calls_edges_match_golden_exactly() {
    let dir = build();
    let db_path = dir.path().join("codegraph.db");

    let ours = multiset(edge_keys(&db_path, "calls"));
    let golden = multiset(edge_keys(&golden_db(), "calls"));

    let our_set: BTreeSet<_> = ours.iter().cloned().collect();
    let golden_set: BTreeSet<_> = golden.iter().cloned().collect();

    let missing: Vec<_> = golden_set.difference(&our_set).collect();
    let extra: Vec<_> = our_set.difference(&golden_set).collect();

    assert!(
        missing.is_empty(),
        "missing golden calls edges (recall gap): {missing:#?}"
    );
    assert!(
        extra.is_empty(),
        "spurious calls edges (precision gap): {extra:#?}"
    );
    // Same count too (guards duplicate call sites like Point::new ×2).
    assert_eq!(ours, golden, "calls edge multiset must equal golden");
    assert_eq!(
        golden.len(),
        15,
        "fixture is expected to have 15 calls edges"
    );
}

/// `instantiates` edges are reproduced exactly.
#[test]
fn instantiates_edges_match_golden_exactly() {
    let dir = build();
    let db_path = dir.path().join("codegraph.db");
    let ours = multiset(edge_keys(&db_path, "instantiates"));
    let golden = multiset(edge_keys(&golden_db(), "instantiates"));
    assert_eq!(ours, golden, "instantiates edges must equal golden");
    assert_eq!(golden.len(), 4);
}

/// `imports` edges are reproduced exactly (file → import-node, one per
/// from/use/import-from statement; plain Python `import X` gets none).
#[test]
fn imports_edges_match_golden_exactly() {
    let dir = build();
    let db_path = dir.path().join("codegraph.db");
    let ours = multiset(edge_keys(&db_path, "imports"));
    let golden = multiset(edge_keys(&golden_db(), "imports"));
    assert_eq!(ours, golden, "imports edges must equal golden");
    assert_eq!(golden.len(), 5);
}

/// `references` parity: every reference we emit is one CodeGraph also emits
/// (no spurious references), and the only golden references we DON'T reproduce
/// are the documented CodeGraph return-type duplicate emissions.
///
/// CodeGraph 0.9.7 emits the return-type `Scalar` reference **twice** for five
/// TS signatures whose return type is nested in a generic (`Array<…>` /
/// `Promise<…>`). We emit each type occurrence exactly once (deterministic, no
/// duplicates), so those five second-copies are absent. Every other golden
/// reference is reproduced.
#[test]
fn references_are_a_subset_modulo_documented_duplicates() {
    let dir = build();
    let db_path = dir.path().join("codegraph.db");

    let ours = multiset(edge_keys(&db_path, "references"));
    let golden = multiset(edge_keys(&golden_db(), "references"));

    let our_set: BTreeSet<_> = ours.iter().cloned().collect();

    // No spurious references (precision): everything we emit, CodeGraph emits.
    let golden_set: BTreeSet<_> = golden.iter().cloned().collect();
    let extra: Vec<_> = our_set.difference(&golden_set).collect();
    assert!(
        extra.is_empty(),
        "spurious references vs golden: {extra:#?}"
    );

    // The golden references we omit are exactly the 5 return-type duplicates:
    // golden has them at multiplicity 2 where we have multiplicity 1.
    let mut deficit = 0usize;
    for key in golden_set.iter() {
        let g = golden.iter().filter(|k| *k == key).count();
        let o = ours.iter().filter(|k| *k == key).count();
        assert!(
            o <= g,
            "over-emitted reference {key:?}: ours {o} > golden {g}"
        );
        deficit += g - o;
    }
    assert_eq!(
        deficit,
        5,
        "expected exactly 5 un-reproduced golden references (return-type \
         duplicate emissions); got {deficit}. ours={} golden={}",
        ours.len(),
        golden.len()
    );
}
