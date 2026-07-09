//! Offline parity check (issue #78 Phase 2c): index `tests/fixtures/graph-src`
//! with semgraph and diff it against the committed CodeGraph 0.9.7 golden
//! (`tests/fixtures/codegraph-v4.db`), applying the committed whitelist.
//!
//! This is the **CI-runnable, offline** acceptance gate: it needs no Node /
//! CodeGraph install, only the two committed fixtures. It asserts the fixture
//! clears the issue's Phase 2 thresholds (≥95% nodes, ≥90% calls) — in fact
//! 100% post-P2b — and that the only non-matching diffs are the ones the
//! whitelist accounts for (the `is_async`/docstring improvements and the
//! CodeGraph return-type duplicate references).
//!
//! Contributors: see `docs/parity-harness.md` for how to run the harness in
//! **live** mode (shelling out to a local codegraph@0.9.7) and how to add a
//! language to this acceptance flow.

use std::path::{Path, PathBuf};

use semgraph::parity::{compare, extract_parity, CompareOptions, Whitelist};
use semgraph::{index_roots, IndexOptions};

fn fixtures() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/src/semgraph
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn require(path: &Path) -> PathBuf {
    assert!(
        path.exists(),
        "required fixture missing: {} — see tests/fixtures/README.md",
        path.display()
    );
    path.to_path_buf()
}

/// Build both graphs and compare with the committed whitelist.
fn run() -> semgraph::ParityReport {
    let fx = fixtures();
    let graph_src = require(&fx.join("graph-src"));
    let golden_db = require(&fx.join("codegraph-v4.db"));
    let whitelist_path = require(&fx.join("parity-whitelist.json"));

    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("semgraph.db");
    index_roots(&[graph_src], &db, &IndexOptions::default()).expect("semgraph index");

    let ours = extract_parity(&db).expect("read semgraph db");
    let golden = extract_parity(&golden_db).expect("read golden db");
    let whitelist = Whitelist::load(&whitelist_path).expect("load whitelist");

    compare(&ours, &golden, &whitelist, &CompareOptions::default())
}

#[test]
fn fixture_clears_acceptance_thresholds() {
    let report = run();
    // Post-P2b the fixture is exact, so we assert the strong invariant while
    // gating on the issue's actual thresholds.
    assert!(
        report.passes(95.0, 90.0),
        "fixture must clear ≥95% nodes / ≥90% calls; got nodes={:.2}% calls={:?}",
        report.node_match_pct(),
        report.calls_match_pct()
    );
    assert_eq!(
        report.node_match_pct(),
        100.0,
        "fixture node parity is exact"
    );
    assert_eq!(
        report.calls_match_pct(),
        Some(100.0),
        "fixture calls parity is exact"
    );
    // The fixture aligns exactly, so there are no reconvention pairs to normalize.
    assert_eq!(
        report.node_total.reconvention, 0,
        "fixture qns align; no reconvention expected"
    );
}

#[test]
fn every_uncounted_diff_is_whitelisted() {
    let report = run();

    // No node is missing/extra at all (exact node parity).
    let counted_missing_nodes = report
        .missing_nodes
        .iter()
        .filter(|d| !d.whitelisted)
        .count();
    let counted_extra_nodes = report.extra_nodes.iter().filter(|d| !d.whitelisted).count();
    assert_eq!(counted_missing_nodes, 0, "unexpected missing nodes");
    assert_eq!(counted_extra_nodes, 0, "unexpected extra nodes");

    // Every non-matching edge and attribute delta must be whitelisted.
    let counted_missing_edges = report
        .missing_edges
        .iter()
        .filter(|d| !d.whitelisted)
        .count();
    let counted_extra_edges = report.extra_edges.iter().filter(|d| !d.whitelisted).count();
    let counted_attrs = report.attr_deltas.iter().filter(|d| !d.whitelisted).count();
    assert_eq!(counted_missing_edges, 0, "un-whitelisted missing edges");
    assert_eq!(counted_extra_edges, 0, "un-whitelisted extra edges");
    assert_eq!(counted_attrs, 0, "un-whitelisted attribute deltas");

    // Every whitelisted diff carries a justification.
    for d in report
        .missing_edges
        .iter()
        .chain(&report.attr_deltas)
        .filter(|d| d.whitelisted)
    {
        assert!(
            d.justification.as_ref().is_some_and(|j| !j.is_empty()),
            "whitelisted diff without justification: {}",
            d.description
        );
    }
}

#[test]
fn whitelist_accounts_for_the_three_adr_categories() {
    let report = run();

    // 1. CodeGraph return-type duplicate references (ADR-004): exactly 5.
    let refs = report
        .edges_by_kind
        .iter()
        .find(|g| g.label == "references")
        .expect("references group present");
    assert_eq!(
        refs.whitelisted_missing, 5,
        "expected 5 whitelisted duplicate references, got {}",
        refs.whitelisted_missing
    );

    // 2. is_async correctness (ADR-003): Rust + Python async defs, whitelisted.
    let is_async: Vec<_> = report
        .attr_deltas
        .iter()
        .filter(|d| d.description.contains("[is_async]"))
        .collect();
    assert!(
        !is_async.is_empty() && is_async.iter().all(|d| d.whitelisted),
        "is_async deltas must all be whitelisted: {is_async:?}"
    );

    // 3. docstring cleanups (ADR-003): present and all whitelisted.
    let docstrings: Vec<_> = report
        .attr_deltas
        .iter()
        .filter(|d| d.description.contains("[docstring]"))
        .collect();
    assert!(
        !docstrings.is_empty() && docstrings.iter().all(|d| d.whitelisted),
        "docstring deltas must all be whitelisted"
    );
}

#[test]
fn without_whitelist_the_known_better_deltas_still_do_not_break_node_or_calls_recall() {
    // The whitelist matters for references/attribute reporting, but node and
    // calls recall are exact regardless — proves the thresholds don't secretly
    // depend on the whitelist for THIS fixture.
    let fx = fixtures();
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("semgraph.db");
    index_roots(&[fx.join("graph-src")], &db, &IndexOptions::default()).unwrap();
    let ours = extract_parity(&db).unwrap();
    let golden = extract_parity(&fx.join("codegraph-v4.db")).unwrap();

    let report = compare(
        &ours,
        &golden,
        &Whitelist::default(),
        &CompareOptions::default(),
    );
    assert_eq!(report.node_match_pct(), 100.0);
    assert_eq!(report.calls_match_pct(), Some(100.0));
    // But references now show an (un-whitelisted) deficit of 5.
    let refs = report
        .edges_by_kind
        .iter()
        .find(|g| g.label == "references")
        .unwrap();
    assert_eq!(refs.missing, 5);
    assert_eq!(refs.whitelisted_missing, 0);
}
