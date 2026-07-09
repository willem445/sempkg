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

/// The exact `(kind, qualified_name, file_path)` keyset of a database.
fn node_keyset(db_path: &Path) -> std::collections::BTreeSet<(String, String, String)> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT kind, qualified_name, file_path FROM nodes")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

/// `file_path -> (start_line, end_line)` for every `file` node.
fn file_spans(db_path: &Path) -> HashMap<String, (i64, i64)> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT file_path, start_line, end_line FROM nodes WHERE kind='file'")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

/// The `signature` of a node identified by `(qualified_name, file_path)`.
fn signature_of(db_path: &Path, qn: &str, file: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT signature FROM nodes WHERE qualified_name=?1 AND file_path=?2",
        rusqlite::params![qn, file],
        |r| r.get::<_, Option<String>>(0),
    )
    .unwrap()
}

/// The `docstring` of a node identified by `(qualified_name, file_path)`.
fn docstring_of(db_path: &Path, qn: &str, file: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT docstring FROM nodes WHERE qualified_name=?1 AND file_path=?2",
        rusqlite::params![qn, file],
        |r| r.get::<_, Option<String>>(0),
    )
    .unwrap()
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

    // 1. EXACT per-kind counts vs the golden CodeGraph fixture.
    let ours = kind_counts(&db_path);
    let golden = kind_counts(&golden_db());
    assert_eq!(
        ours, golden,
        "per-kind node counts must match the golden fixture exactly"
    );

    // 2. EXACT (kind, qualified_name, file_path) keyset vs golden.
    let our_keys = node_keyset(&db_path);
    let golden_keys = node_keyset(&golden_db());
    let missing: Vec<_> = golden_keys.difference(&our_keys).collect();
    let extra: Vec<_> = our_keys.difference(&golden_keys).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "node keyset diverges from golden.\n  missing (in golden, not ours): {missing:?}\n  extra (in ours, not golden): {extra:?}"
    );

    // 3. EXACT file-node spans vs golden (guards the F2 trailing-newline fix).
    let our_spans = file_spans(&db_path);
    let golden_spans = file_spans(&golden_db());
    assert_eq!(
        our_spans, golden_spans,
        "file-node line spans must match golden"
    );

    // 4. Pinned SIGNATURES — byte-exact vs what CodeGraph stores (F3).
    let sig_pins = [
        (
            "circle_area",
            "python/shapes.py",
            "(radius: Scalar) -> Scalar",
        ),
        (
            "Report::measure",
            "python/main.py",
            "(self, radius: Scalar) -> Scalar",
        ),
        ("Scalar", "python/shapes.py", "= float"), // variable assignment tail
        (
            "hypot",
            "rust/geometry.rs",
            "(a: Scalar, b: Scalar) -> Scalar",
        ),
        (
            "Point::new",
            "rust/geometry.rs",
            "(x: Scalar, y: Scalar) -> Self",
        ),
        (
            "shapes",
            "python/main.py",
            "from shapes import Circle, Kind, Scalar, circle_area",
        ),
        // Multi-line signature preserved (newlines normalized to \n).
        (
            "fetchAndMeasure",
            "typescript/index.ts",
            "(\n  points: Array<[Scalar, Scalar]>,\n): Promise<Scalar>",
        ),
    ];
    for (qn, file, sig) in sig_pins {
        assert_eq!(
            signature_of(&db_path, qn, file).as_deref(),
            Some(sig),
            "signature parity for {qn} in {file}"
        );
    }

    // Types/members carry NO signature, exactly like CodeGraph.
    for (qn, file) in [
        ("Point", "rust/geometry.rs"),
        ("Shape", "rust/geometry.rs"),
        ("Report", "python/main.py"),
        ("Scalar", "rust/geometry.rs"),
    ] {
        assert_eq!(
            signature_of(&db_path, qn, file),
            None,
            "{qn} ({file}) must have NULL signature"
        );
    }

    // 5. Pinned DOCSTRINGS. Where 0.9.7 also stores one (TS direct-sibling
    //    comment) we match it byte-for-byte; where we deliberately improve on
    //    0.9.7 (Python populated, Rust cleaned, no module-doc bleed) we pin our
    //    value — the P2c parity harness whitelists these (see parse.rs).
    assert_eq!(
        docstring_of(&db_path, "Point::distanceTo", "typescript/geometry.ts").as_deref(),
        Some("Method that calls a free function in this file (intra-file call)."),
        "TS direct-sibling comment matches 0.9.7 exactly",
    );
    assert_eq!(
        docstring_of(&db_path, "hypot", "rust/geometry.rs").as_deref(),
        Some("Free function used by `Point::distance_to`."),
        "Rust /// captured cleanly (no stray leading slash)",
    );
    assert_eq!(
        docstring_of(&db_path, "Scalar", "rust/geometry.rs").as_deref(),
        Some("A type alias over a primitive — the reader must record `type_alias` nodes."),
        "Rust type_alias docstring has NO module-//!-doc bleed (improvement)",
    );
    assert_eq!(
        docstring_of(&db_path, "summarize", "python/main.py").as_deref(),
        Some("Plain function calling a function defined in another file."),
        "Python docstring populated (0.9.7 leaves NULL — deliberate improvement)",
    );

    // 6. Pinned symbols still exact on kind + line range + file.
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

/// F6: a file that is not valid UTF-8 must be recorded with an errored `files`
/// row, not silently dropped.
#[test]
fn non_utf8_file_is_recorded_with_errors() {
    let base = tempfile::TempDir::new().unwrap();
    let root = base.path().join("src");
    std::fs::create_dir_all(&root).unwrap();
    // A `.rs` file with invalid UTF-8 bytes.
    std::fs::write(root.join("good.rs"), "pub fn ok_fn() {}\n").unwrap();
    std::fs::write(root.join("bad.rs"), [0xff, 0xfe, 0x00, 0x9f, b'x']).unwrap();

    let db_path = base.path().join("codegraph.db");
    let stats = index_roots(&[root], &db_path, &IndexOptions::default()).unwrap();
    assert_eq!(
        stats.file_count, 2,
        "both files recorded (bad one not dropped)"
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // Single root → paths are relative to it (no namespace prefix).
    let errors: Option<String> = conn
        .query_row("SELECT errors FROM files WHERE path='bad.rs'", [], |r| {
            r.get(0)
        })
        .expect("bad.rs must have a files row");
    let errors = errors.expect("errors column must be populated");
    assert!(
        errors.contains("UTF-8"),
        "errors should mention the UTF-8 failure, got {errors}"
    );
    // The good file still parsed normally.
    assert!(GraphDb::open(&db_path)
        .unwrap()
        .query("ok_fn", None, 5)
        .unwrap()
        .iter()
        .any(|n| n.name == "ok_fn"));
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
