//! Round-trip + parity + multi-root (#79) integration tests for the writer.
//!
//! These drive the full write path (`index_roots`) and read the result back
//! with the *existing* reader (`GraphDb`), proving the writer produces a
//! schema-v4 database the reader understands unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use semgraph::{index_roots, GraphDb, IndexOptions};

fn graph_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/graph-src")
}

fn golden_db() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/codegraph-v4.db")
}

/// Kind → count from a graph database's `nodes` table.
fn kind_counts(db_path: &Path) -> HashMap<String, i64> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT kind, COUNT(*) FROM nodes GROUP BY kind")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

#[test]
fn writes_a_reader_compatible_schema_v4_db() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("codegraph.db");
    let stats = index_roots(&[graph_src()], &db_path, &IndexOptions::default()).unwrap();
    assert!(
        stats.file_count == 7,
        "expected 7 files, got {}",
        stats.file_count
    );

    // The existing reader must open our DB and report schema v4.
    let db = GraphDb::open(&db_path).unwrap();
    assert_eq!(db.schema_version(), 4);
    let status = db.status().unwrap();
    assert_eq!(status.schema_version, 4);
    assert_eq!(status.file_count, 7);
    assert_eq!(status.node_count as usize, stats.node_count);
}

#[test]
fn smoke_parity_against_codegraph_fixture() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("codegraph.db");
    index_roots(&[graph_src()], &db_path, &IndexOptions::default()).unwrap();

    let ours = kind_counts(&db_path);
    let golden = kind_counts(&golden_db());

    // Every node kind CodeGraph produced must be present in our output.
    for kind in golden.keys() {
        assert!(
            ours.contains_key(kind),
            "kind '{kind}' missing from writer output; got {ours:?}"
        );
    }

    // Total node count within a sensible tolerance of the golden fixture (55).
    let our_total: i64 = ours.values().sum();
    let golden_total: i64 = golden.values().sum();
    let lo = golden_total - golden_total / 5; // -20%
    let hi = golden_total + golden_total / 5; // +20%
    assert!(
        (lo..=hi).contains(&our_total),
        "node count {our_total} outside tolerance [{lo},{hi}] of golden {golden_total}"
    );

    // Exact match on a handful of pinned symbols: kind + line range + file.
    let db = GraphDb::open(&db_path).unwrap();
    let pins = [
        ("circle_area", "function", "python/shapes.py", 34u32, 36u32),
        ("Point::new", "method", "rust/geometry.rs", 17, 19),
        ("Shape::area", "method", "rust/geometry.rs", 38, 44),
        ("hypot", "function", "rust/geometry.rs", 48, 50),
        (
            "Kind::Circle",
            "enum_member",
            "typescript/geometry.ts",
            11,
            11,
        ),
        ("fetchAndMeasure", "function", "typescript/index.ts", 33, 38),
    ];
    for (qn, kind, file, sl, el) in pins {
        let hits = db.query(qn, Some(kind), 10).unwrap();
        let hit = hits
            .iter()
            .find(|n| n.qualified_name == qn && n.file_path == file)
            .unwrap_or_else(|| panic!("pinned symbol {qn} ({kind}) not found in {file}: {hits:?}"));
        assert_eq!(hit.start_line, sl, "{qn} start_line");
        assert_eq!(hit.end_line, el, "{qn} end_line");
    }
}

#[test]
fn contains_edges_and_file_nodes_are_emitted() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("codegraph.db");
    index_roots(&[graph_src()], &db_path, &IndexOptions::default()).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // A file node per source file.
    let file_nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes WHERE kind='file'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(file_nodes, 7);
    // Only `contains` edges in Phase 2a (resolution is Phase 2b).
    let non_contains: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE kind != 'contains'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(non_contains, 0);
    let contains: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE kind='contains'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        contains > 40,
        "expected many contains edges, got {contains}"
    );
}

/// The #79 acceptance test: two source roots must both land in ONE database,
/// with unambiguous paths — neither root silently overwrites the other.
#[test]
fn issue_79_two_distinct_roots_land_in_one_db() {
    let base = tempfile::TempDir::new().unwrap();
    let root_a = base.path().join("src");
    let root_b = base.path().join("radar_src");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    std::fs::write(root_a.join("alpha.rs"), "pub fn alpha_fn() {}\n").unwrap();
    std::fs::write(root_b.join("beta.py"), "def beta_fn():\n    return 1\n").unwrap();

    let db_path = base.path().join("codegraph.db");
    let stats = index_roots(
        &[root_a.clone(), root_b.clone()],
        &db_path,
        &IndexOptions::default(),
    )
    .unwrap();
    assert_eq!(stats.file_count, 2, "both roots' files must be indexed");

    let db = GraphDb::open(&db_path).unwrap();
    // Both roots' symbols are present.
    assert!(
        db.query("alpha_fn", None, 5)
            .unwrap()
            .iter()
            .any(|n| n.name == "alpha_fn"),
        "root A symbol missing"
    );
    assert!(
        db.query("beta_fn", None, 5)
            .unwrap()
            .iter()
            .any(|n| n.name == "beta_fn"),
        "root B symbol missing"
    );

    // Paths are namespaced by root basename and unambiguous.
    let paths = db.file_paths().unwrap();
    assert!(paths.contains(&"src/alpha.rs".to_string()), "got {paths:?}");
    assert!(
        paths.contains(&"radar_src/beta.py".to_string()),
        "got {paths:?}"
    );
}

/// #79, harder case: two roots that share a basename (`src`) must still be
/// disambiguated (steering note) — both land in one DB with distinct paths.
#[test]
fn issue_79_two_roots_with_same_basename_are_disambiguated() {
    let base = tempfile::TempDir::new().unwrap();
    let backend = base.path().join("backend").join("src");
    let frontend = base.path().join("frontend").join("src");
    std::fs::create_dir_all(&backend).unwrap();
    std::fs::create_dir_all(&frontend).unwrap();
    // Same relative filename under each root — this is what collides in #79.
    std::fs::write(backend.join("lib.rs"), "pub fn backend_only() {}\n").unwrap();
    std::fs::write(frontend.join("lib.rs"), "pub fn frontend_only() {}\n").unwrap();

    let db_path = base.path().join("codegraph.db");
    let stats = index_roots(
        &[backend.clone(), frontend.clone()],
        &db_path,
        &IndexOptions::default(),
    )
    .unwrap();
    assert_eq!(stats.file_count, 2);

    let db = GraphDb::open(&db_path).unwrap();
    assert!(db
        .query("backend_only", None, 5)
        .unwrap()
        .iter()
        .any(|n| n.name == "backend_only"));
    assert!(db
        .query("frontend_only", None, 5)
        .unwrap()
        .iter()
        .any(|n| n.name == "frontend_only"));

    let paths = db.file_paths().unwrap();
    assert!(
        paths.contains(&"backend/src/lib.rs".to_string()),
        "got {paths:?}"
    );
    assert!(
        paths.contains(&"frontend/src/lib.rs".to_string()),
        "got {paths:?}"
    );
    // The two same-named files did NOT collide onto one path.
    assert_eq!(paths.iter().filter(|p| p.ends_with("lib.rs")).count(), 2);
}
