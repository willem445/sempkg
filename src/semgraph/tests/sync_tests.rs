//! Incremental-sync tests (issue #78, Phase 2b).
//!
//! Every scenario (no-op / modify-callee / modify-caller / add / delete /
//! rename / competing-definition multiplicity flip) asserts the same invariant:
//! after [`sync`], the database is
//! **canonically equal** to a from-scratch [`index_roots`] of the same tree —
//! identical nodes, edges, and files, ignoring only the autoincrement
//! `edges.id` and the wall-clock `updated_at` / `modified_at` / `indexed_at`
//! columns. The FTS index and SQLite integrity are also checked after each sync.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use semgraph::{index_roots, sync, IndexOptions};

/// Recursively copy a directory tree.
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

fn fixture_tree() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/graph-src")
}

/// A canonical, order-independent snapshot of a graph database's *content*,
/// excluding volatile columns (autoincrement ids, timestamps).
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    /// node rows minus `updated_at`, sorted.
    nodes: Vec<String>,
    /// edge rows minus `id`, sorted (a multiset, so duplicate call sites count).
    edges: Vec<String>,
    /// file rows minus `modified_at`/`indexed_at`, sorted.
    files: Vec<String>,
}

fn snapshot(db_path: &Path) -> Snapshot {
    let conn = Connection::open(db_path).unwrap();

    let mut nstmt = conn
        .prepare(
            "SELECT id, kind, name, qualified_name, file_path, language, start_line, end_line, \
                    start_column, end_column, IFNULL(docstring,''), IFNULL(signature,''), \
                    IFNULL(visibility,''), is_exported, is_async, is_static, is_abstract, \
                    IFNULL(decorators,''), IFNULL(type_parameters,'') \
             FROM nodes ORDER BY id",
        )
        .unwrap();
    let nodes: Vec<String> = nstmt
        .query_map([], |r| {
            let mut parts = Vec::new();
            for i in 0..19 {
                parts.push(r.get::<_, rusqlite::types::Value>(i).map(fmt_val)?);
            }
            Ok(parts.join("|"))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    let mut estmt = conn
        .prepare(
            "SELECT source, target, kind, IFNULL(metadata,''), IFNULL(line,-1), IFNULL(col,-1), \
                    IFNULL(provenance,'') FROM edges",
        )
        .unwrap();
    let mut edges: Vec<String> = estmt
        .query_map([], |r| {
            let mut parts = Vec::new();
            for i in 0..7 {
                parts.push(r.get::<_, rusqlite::types::Value>(i).map(fmt_val)?);
            }
            Ok(parts.join("|"))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    edges.sort();

    let mut fstmt = conn
        .prepare(
            "SELECT path, content_hash, language, size, node_count, IFNULL(errors,'') \
             FROM files ORDER BY path",
        )
        .unwrap();
    let files: Vec<String> = fstmt
        .query_map([], |r| {
            let mut parts = Vec::new();
            for i in 0..6 {
                parts.push(r.get::<_, rusqlite::types::Value>(i).map(fmt_val)?);
            }
            Ok(parts.join("|"))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    Snapshot {
        nodes,
        edges,
        files,
    }
}

fn fmt_val(v: rusqlite::types::Value) -> String {
    use rusqlite::types::Value;
    match v {
        Value::Null => "∅".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) => s,
        Value::Blob(_) => "<blob>".to_string(),
    }
}

/// Assert the FTS index mirrors `nodes` and SQLite integrity is clean.
fn assert_healthy(db_path: &Path) {
    let conn = Connection::open(db_path).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok", "integrity_check must be clean");

    // The contentless-external FTS table must have exactly one row per node.
    let nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    let fts: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        fts, nodes,
        "nodes_fts row count must equal nodes (triggers)"
    );

    // The FTS5 built-in integrity check throws on any index/content mismatch.
    conn.execute_batch("INSERT INTO nodes_fts(nodes_fts) VALUES('integrity-check');")
        .expect("nodes_fts integrity-check must pass");
}

/// Build a fresh temp copy of the fixture tree and index it. Returns the temp
/// dir (source under `src/`), the db path, and the source root.
fn fresh_indexed() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("src");
    copy_tree(&fixture_tree(), &root);
    let db = dir.path().join("codegraph.db");
    index_roots(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();
    (dir, db, root)
}

/// Index a (possibly modified) tree from scratch into its own db, for the
/// equality oracle.
fn index_scratch(root: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("scratch.db");
    index_roots(&[root.to_path_buf()], &db, &IndexOptions::default()).unwrap();
    (dir, db)
}

/// After the edit described by `mutate`, `sync` of the incrementally-maintained
/// db must equal a from-scratch index of the same tree.
fn assert_sync_equals_scratch(mutate: impl FnOnce(&Path)) {
    let (_dir, db, root) = fresh_indexed();
    mutate(&root);

    sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();
    assert_healthy(&db);

    let (_sdir, scratch) = index_scratch(&root);
    let synced = snapshot(&db);
    let scratched = snapshot(&scratch);
    if synced != scratched {
        diff_lines("NODES", &synced.nodes, &scratched.nodes);
        diff_lines("EDGES", &synced.edges, &scratched.edges);
        diff_lines("FILES", &synced.files, &scratched.files);
    }
    assert_eq!(
        synced, scratched,
        "sync must be canonically equal to a from-scratch index"
    );
}

/// Print rows present in only one side (synced vs scratch) for debugging.
fn diff_lines(label: &str, synced: &[String], scratch: &[String]) {
    use std::collections::BTreeSet;
    let a: BTreeSet<_> = synced.iter().collect();
    let b: BTreeSet<_> = scratch.iter().collect();
    for x in a.difference(&b) {
        println!("[{label}] only in SYNC:    {x}");
    }
    for x in b.difference(&a) {
        println!("[{label}] only in SCRATCH: {x}");
    }
}

#[test]
fn noop_sync_is_stable() {
    assert_sync_equals_scratch(|_root| {
        // no change at all
    });
}

#[test]
fn modify_callee_file_reresolves_dependents() {
    // Rename the free function `hypot` → `hypot_renamed` in the Rust callee.
    // From-scratch, lib.rs's calls into `hypot` disappear; sync must match by
    // re-resolving lib.rs (which had edges into geometry.rs).
    assert_sync_equals_scratch(|root| {
        let p = root.join("rust/geometry.rs");
        let s = fs::read_to_string(&p)
            .unwrap()
            .replace("hypot", "hypot_renamed");
        fs::write(&p, s).unwrap();
    });
}

#[test]
fn modify_caller_file_recomputes_its_edges() {
    // Add a new cross-file call in the caller only.
    assert_sync_equals_scratch(|root| {
        let p = root.join("rust/lib.rs");
        let mut s = fs::read_to_string(&p).unwrap();
        s.push_str("\npub fn extra(p: &[(Scalar, Scalar)]) -> Scalar { total_distance(p) }\n");
        fs::write(&p, s).unwrap();
    });
}

#[test]
fn add_new_file_that_calls_existing() {
    assert_sync_equals_scratch(|root| {
        // A new Rust file in the same dir that calls into geometry.rs.
        fs::write(
            root.join("rust/extra.rs"),
            "use geometry::hypot;\npub fn extra_len(a: f64, b: f64) -> f64 { hypot(a, b) }\n",
        )
        .unwrap();
    });
}

#[test]
fn delete_file_removes_nodes_and_dependent_edges() {
    assert_sync_equals_scratch(|root| {
        fs::remove_file(root.join("python/shapes.py")).unwrap();
    });
}

#[test]
fn rename_file_is_delete_plus_add() {
    assert_sync_equals_scratch(|root| {
        let from = root.join("typescript/geometry.ts");
        let to = root.join("typescript/geom2.ts");
        let s = fs::read_to_string(&from).unwrap();
        fs::write(&to, s).unwrap();
        fs::remove_file(&from).unwrap();
    });
}

#[test]
fn sync_without_existing_db_is_a_full_index() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("src");
    copy_tree(&fixture_tree(), &root);
    let db = dir.path().join("codegraph.db");
    // No prior index — sync should behave exactly like index_roots.
    sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();
    assert_healthy(&db);

    let (_sdir, scratch) = index_scratch(&root);
    assert_eq!(snapshot(&db), snapshot(&scratch));
}

#[test]
fn repeated_syncs_are_idempotent() {
    let (_dir, db, root) = fresh_indexed();
    // A change, then two syncs; the second must be a no-op that changes nothing.
    let p = root.join("python/main.py");
    let mut s = fs::read_to_string(&p).unwrap();
    s.push_str("\ndef added_fn():\n    return summarize([])\n");
    fs::write(&p, s).unwrap();

    sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();
    let after_first = snapshot(&db);
    sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();
    let after_second = snapshot(&db);
    assert_eq!(
        after_first, after_second,
        "a second sync with no disk change must not alter the db"
    );
    assert_healthy(&db);
}

/// A focused check that node deletion keeps `nodes_fts` consistent: index,
/// delete a file via sync, and prove the FTS index shrank in lock-step with
/// `nodes` (no orphan rows) while survivors remain searchable. `assert_healthy`
/// runs the FTS5 built-in `integrity-check`, which fails on any orphan.
#[test]
fn deletion_keeps_fts_in_sync() {
    let (_dir, db, root) = fresh_indexed();
    let node_count = |db: &Path| -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap()
    };
    let before = node_count(&db);

    fs::remove_file(root.join("python/shapes.py")).unwrap();
    sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();

    let after = node_count(&db);
    assert!(after < before, "deleting a file must drop nodes");
    // Triggers must have kept nodes_fts in lockstep; assert_healthy verifies
    // count parity AND runs the FTS5 integrity-check (fails on any orphan row).
    assert_healthy(&db);

    // A survivor in another file is still searchable via FTS.
    let conn = Connection::open(&db).unwrap();
    let hypot_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes_fts WHERE nodes_fts MATCH 'hypot'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(hypot_hits > 0, "survivor still searchable in FTS");
}

/// Guard the invalidation direction explicitly: modifying a callee must update
/// the *caller's* edge set even though the caller file was not itself edited.
#[test]
fn callee_change_updates_unedited_caller_edges() {
    let (_dir, db, root) = fresh_indexed();

    // Baseline: lib.rs::describe calls geometry.rs::hypot.
    let describe_calls_hypot = |db: &Path| -> bool {
        let conn = Connection::open(db).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM edges e \
             JOIN nodes s ON s.id=e.source JOIN nodes t ON t.id=e.target \
             WHERE e.kind='calls' AND s.qualified_name='describe' AND t.name='hypot'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    };
    assert!(describe_calls_hypot(&db), "baseline edge present");

    // Rename the *definition* of hypot in the callee only. The caller's source
    // is untouched — `describe` still literally calls `hypot(...)`, which is now
    // undefined — so from-scratch (and thus sync) drops that call edge entirely.
    let p = root.join("rust/geometry.rs");
    let s = fs::read_to_string(&p)
        .unwrap()
        .replace("fn hypot", "fn hypotenuse");
    fs::write(&p, s).unwrap();
    sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();

    assert!(
        !describe_calls_hypot(&db),
        "caller's stale edge into the renamed callee must be re-resolved away"
    );
    // Cross-check against a from-scratch index of the same modified tree.
    let (_sdir, scratch) = index_scratch(&root);
    assert_eq!(
        snapshot(&db),
        snapshot(&scratch),
        "callee rename: sync must equal from-scratch"
    );
    assert_healthy(&db);
}

/// Case (c) — the hardest invalidation path: editing file A changes the *global
/// multiplicity* of a symbol name, which must flip an edge between two OTHER
/// files (B → C) that A never references directly. B is unchanged on disk, yet
/// its edge must be recomputed. This exercises the `delta_names` clause of the
/// invalidation set (target-file-only tracing would miss it). Sync must equal a
/// from-scratch index.
#[test]
fn competing_definition_flips_edge_between_other_files() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("src");
    let pkg = root.join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    // C defines `target`; B calls it. No competitor yet → `target` is unique, so
    // B → C resolves.
    fs::write(pkg.join("c.py"), "def target():\n    return 1\n").unwrap();
    fs::write(pkg.join("b.py"), "def caller():\n    return target()\n").unwrap();
    let db = dir.path().join("codegraph.db");
    index_roots(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();

    // Where does B's `caller` resolve its `target()` call?
    let caller_target_files = |db: &Path| -> Vec<String> {
        let conn = Connection::open(db).unwrap();
        let mut st = conn
            .prepare(
                "SELECT t.file_path FROM edges e \
                 JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target \
                 WHERE e.kind='calls' AND s.qualified_name='caller' AND t.name='target'",
            )
            .unwrap();
        let mut v: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        caller_target_files(&db),
        vec!["pkg/c.py".to_string()],
        "baseline: B → C edge present (target is globally unique)"
    );

    // Add a COMPETING `target` in a new file A in the same package. `target` is
    // now ambiguous (A and C, same dir), so B's call no longer resolves — the
    // B → C edge must disappear, though A never mentions B or C.
    fs::write(pkg.join("a.py"), "def target():\n    return 2\n").unwrap();
    sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();

    assert!(
        caller_target_files(&db).is_empty(),
        "competing definition must drop B's now-ambiguous call edge, got {:?}",
        caller_target_files(&db)
    );
    assert_healthy(&db);

    // And the whole DB equals a from-scratch index of the same tree.
    let scratch = dir.path().join("scratch.db");
    index_roots(
        std::slice::from_ref(&root),
        &scratch,
        &IndexOptions::default(),
    )
    .unwrap();
    assert_eq!(
        snapshot(&db),
        snapshot(&scratch),
        "case-(c) multiplicity flip: sync must equal from-scratch"
    );
}
