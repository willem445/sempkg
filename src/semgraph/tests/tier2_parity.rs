//! Node/edge parity for the tier-2 language packs (issue #78, Phase 2c) vs the
//! committed CodeGraph 0.9.7 goldens under `tests/fixtures/graph-src-tier2/`.
//!
//! Each language is indexed independently (`index_roots` over its fixture dir)
//! and compared to its own golden `codegraph-v4-<lang>.db`:
//!
//! - **nodes** `(kind, qualified_name, file_path)` — EXACT (the ≥95% acceptance
//!   metric; we hold 100% on the fixtures).
//! - **`calls`** `(source_qn, target_qn, line, col)` — EXACT (the ≥90% metric;
//!   we hold 100%).
//! - **`contains` / `imports` / `instantiates`** — EXACT. Construction sites are
//!   genuinely exercised: Java `new Point(...)` (→ 2 `instantiates` to the class)
//!   and C++ `new Point(...)` (→ 1 `instantiates` to the class). C has no
//!   construction syntax and 0.9.7 does not model Go composite literals as
//!   constructions, so those two emit none (asserted, not merely absent).
//! - **`references`** — EXACT for C/C++/Go; for Java, EXACT modulo one documented
//!   CodeGraph type-name→constructor misresolution (we resolve to the type).
//! - **`extends` / `implements`** (issue #78 edge alignment) — EXACT per language:
//!   C++ single + multiple inheritance (`base_class_clause` → `extends`, no
//!   `implements`; the one golden `Point extends Scalar` is a CodeGraph
//!   return-type-misread FABRICATION we drop, whitelisted); Go struct/interface
//!   embedding (`extends`); Java `superclass`/`interfaces`/`extends_interfaces`
//!   (`extends`/`implements`). C has none. CodeGraph's synthesized interface→impl
//!   `calls` (NULL column, `synthesizedBy=interface-impl`) are excluded from the
//!   graded `calls` multiset, matching tier-3 and ADR-005.
//! - **docstrings** — where CodeGraph 0.9.7 is buggy (C++ `///` → stray `/`,
//!   comment bleed) or incomplete (Go type-decl docstrings left NULL), we emit
//!   clean/complete docstrings. These are the same class of *known-better*
//!   deviations ADR-003 already pins for tier-1 (`is_async`, Rust/TS docstrings).
//!
//! All of the above deviations are the P2c whitelist for these languages,
//! asserted explicitly below so a regression in either direction fails.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use semgraph::{index_roots, IndexOptions};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn build(lang: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("codegraph.db");
    let src = fixtures().join("graph-src-tier2").join(lang);
    index_roots(&[src], &db, &IndexOptions::default()).unwrap();
    (dir, db)
}

fn golden(lang: &str) -> PathBuf {
    fixtures().join(format!("codegraph-v4-{lang}.db"))
}

// ---- readers --------------------------------------------------------------

type NodeKey = (String, String, String);

fn node_keys(db: &Path) -> BTreeSet<NodeKey> {
    let conn = rusqlite::Connection::open(db).unwrap();
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

/// An edge as `(source_qn, target_qn, line, col)` for one edge kind.
type EdgeKey = (String, String, i64, i64);

fn edge_keys(db: &Path, kind: &str) -> BTreeSet<EdgeKey> {
    let conn = rusqlite::Connection::open(db).unwrap();
    // `calls` excludes CodeGraph's synthesized interface→impl edges (Phase 5.5
    // heuristic, `synthesizedBy=interface-impl`), which carry a NULL call-site
    // column — a name-based implementation bridge, not a real call site. Matching
    // tier-3's grading and ADR-005; the native indexer does not fabricate them.
    let extra = if kind == "calls" {
        " AND e.line IS NOT NULL AND e.col IS NOT NULL"
    } else {
        ""
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT s.qualified_name, t.qualified_name, COALESCE(e.line,-1), COALESCE(e.col,-1) \
             FROM edges e JOIN nodes s ON s.id=e.source JOIN nodes t ON t.id=e.target \
             WHERE e.kind = ?1{extra}"
        ))
        .unwrap();
    stmt.query_map([kind], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

fn attr(db: &Path, qn: &str, file: &str, col: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        &format!("SELECT {col} FROM nodes WHERE qualified_name=?1 AND file_path=?2"),
        rusqlite::params![qn, file],
        |r| r.get::<_, Option<String>>(0),
    )
    .unwrap()
}

fn flag(db: &Path, qn: &str, file: &str, col: &str) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        &format!("SELECT {col} FROM nodes WHERE qualified_name=?1 AND file_path=?2"),
        rusqlite::params![qn, file],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
}

fn assert_set_eq(what: &str, ours: &BTreeSet<EdgeKey>, expected: &BTreeSet<EdgeKey>) {
    let missing: Vec<_> = expected.difference(ours).collect();
    let extra: Vec<_> = ours.difference(expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{what} mismatch\n  missing: {missing:#?}\n  extra: {extra:#?}"
    );
}

/// Nodes, `contains`, `calls`, `imports`, and `instantiates` are reproduced
/// exactly for every tier-2 language. The inheritance/type-reference families
/// (`extends`/`implements`/`references`) are graded per-language below, since
/// each carries its own small, documented whitelist.
fn assert_core_exact(lang: &str, db: &Path) {
    let g = golden(lang);
    assert_eq!(
        node_keys(db),
        node_keys(&g),
        "{lang}: node keyset must equal golden"
    );
    for kind in ["contains", "calls", "imports", "instantiates"] {
        assert_set_eq(
            &format!("{lang} {kind}"),
            &edge_keys(db, kind),
            &edge_keys(&g, kind),
        );
    }
}

/// Assert our edges of `kind` equal the golden's, after removing the whitelisted
/// `missing` (golden-only, e.g. a CodeGraph fabrication) and `spurious`
/// (ours-only) keys from the respective sides. An empty whitelist means exact.
fn assert_edges_wl(
    lang: &str,
    kind: &str,
    db: &Path,
    missing_wl: &[EdgeKey],
    spurious_wl: &[EdgeKey],
) {
    let mut golden = edge_keys(&golden(lang), kind);
    for m in missing_wl {
        assert!(
            golden.remove(m),
            "{lang} {kind}: whitelisted missing edge {m:?} not in golden (stale whitelist)"
        );
    }
    let mut ours = edge_keys(db, kind);
    for s in spurious_wl {
        assert!(
            ours.remove(s),
            "{lang} {kind}: whitelisted spurious edge {s:?} not emitted (stale whitelist)"
        );
    }
    assert_set_eq(&format!("{lang} {kind} (whitelisted)"), &ours, &golden);
}

/// Assert we emit no edges of `kind` and the golden has none either.
fn assert_kind_empty(lang: &str, kind: &str, db: &Path) {
    assert!(
        edge_keys(db, kind).is_empty(),
        "{lang}: expected no `{kind}` edges from semgraph"
    );
    assert!(
        edge_keys(&golden(lang), kind).is_empty(),
        "{lang}: golden unexpectedly has `{kind}` edges"
    );
}

// ---- C --------------------------------------------------------------------

#[test]
fn c_parity() {
    let (_d, db) = build("c");
    assert_core_exact("c", &db);
    // C has no inheritance and 0.9.7 emits no C type references — neither do we.
    assert_kind_empty("c", "references", &db);
    assert_kind_empty("c", "extends", &db);
    assert_kind_empty("c", "implements", &db);
    // Typedef → type_alias with NULL signature; function signatures are NULL.
    assert_eq!(attr(&db, "Scalar", "geometry.h", "signature"), None);
    assert_eq!(attr(&db, "hypot_scalar", "geometry.c", "signature"), None);
    // Doxygen `/** … */` doc captured; enum members qualified `Shape::…`.
    assert_eq!(
        attr(&db, "Scalar", "geometry.h", "docstring").as_deref(),
        Some("A type alias over a primitive (typedef).")
    );
    // Three cross-file/intra-file `calls` (par with the other languages); C has
    // no construction syntax, so 0.9.7 (and we) emit no `instantiates`.
    assert_eq!(edge_keys(&db, "calls").len(), 3);
    assert!(edge_keys(&golden("c"), "instantiates").is_empty());
    assert!(edge_keys(&db, "instantiates").is_empty());
}

// ---- C++ ------------------------------------------------------------------

#[test]
fn cpp_parity() {
    let (_d, db) = build("cpp");
    assert_core_exact("cpp", &db);
    // C++ has no `interface` kind, so 0.9.7 records no `implements`; and it emits
    // no type references (param/return user types).
    assert_kind_empty("cpp", "implements", &db);
    assert_kind_empty("cpp", "references", &db);

    // `extends`: the three genuine bases in `shapes.hpp` (single inheritance
    // `Disc : Solid`, and multiple inheritance `Prism : Solid, Drawable`) are
    // reproduced exactly. The ONE golden edge we deliberately drop is a CodeGraph
    // 0.9.7 FABRICATION — it misparses `geometry.hpp`'s in-class method return
    // type (`Scalar distanceTo(...) const;`) as a base class, emitting a spurious
    // `class Point extends Scalar`. We never emit that (it is not in a
    // `base_class_clause`); whitelisted as missing.
    let cpp_spurious_extends = ("Point".to_string(), "Scalar".to_string(), 23, 4);
    assert_edges_wl("cpp", "extends", &db, &[cpp_spurious_extends], &[]);

    // WHITELIST 2 — docstrings: 0.9.7 keeps a stray leading `/` on `///` doc
    // comments and bleeds a trailing `// namespace geo` into `main`. We emit
    // clean docstrings (known-better, same class as ADR-003's Rust/TS docs).
    assert_eq!(
        attr(&golden("cpp"), "hypot_scalar", "geometry.cpp", "docstring").as_deref(),
        Some("/ Free function used by Point::distanceTo and across files.")
    );
    assert_eq!(
        attr(&db, "hypot_scalar", "geometry.cpp", "docstring").as_deref(),
        Some("Free function used by Point::distanceTo and across files.")
    );
    assert_eq!(
        attr(&golden("cpp"), "main", "main.cpp", "docstring").as_deref(),
        Some("namespace geo"),
    );
    assert_eq!(attr(&db, "main", "main.cpp", "docstring"), None);

    // Out-of-line method qualified by its `Type::` declarator; signatures NULL.
    assert_eq!(
        attr(&db, "Point::distanceTo", "geometry.cpp", "signature"),
        None
    );

    // `new Point(...)` → exactly one `instantiates` edge to the class (matched
    // exactly by `assert_core_exact`); the constructor's out-of-line definition
    // is in another file, so 0.9.7 resolves the construction to the class node.
    let insts = edge_keys(&db, "instantiates");
    assert_eq!(insts.len(), 1);
    let (src, tgt, ..) = insts.iter().next().unwrap();
    assert_eq!((src.as_str(), tgt.as_str()), ("make_and_measure", "Point"));

    // Three `calls`: two name-based member calls (`pts[i-1].distanceTo`,
    // `p->distanceTo`) and one free call (`hypot_scalar`). The namespace-qualified
    // `geo::hypot_scalar(...)` in `make_and_measure` is DROPPED — 0.9.7 strips
    // namespaces so `geo::hypot_scalar` matches no symbol; we match by dropping it
    // (exercises the qualified-call parse path). Net: 3, same as the golden.
    assert_eq!(edge_keys(&db, "calls").len(), 3);
    assert!(
        !edge_keys(&db, "calls")
            .iter()
            .any(|(s, t, ..)| s == "make_and_measure" && t == "hypot_scalar"),
        "namespace-qualified geo::hypot_scalar call must be dropped, matching 0.9.7"
    );
}

// ---- Go -------------------------------------------------------------------

#[test]
fn go_parity() {
    let (_d, db) = build("go");
    assert_core_exact("go", &db);
    // References (struct types in signatures, aliases excluded) match exactly.
    assert_set_eq(
        "go references",
        &edge_keys(&db, "references"),
        &edge_keys(&golden("go"), "references"),
    );
    // `extends`: Go struct/interface embedding (`embed.go`: `Disc` embeds `Base`,
    // `ReadWriter` embeds `Reader`) → two `extends` edges, reproduced exactly. Go
    // has no explicit conformance, so no `implements`.
    assert_edges_wl("go", "extends", &db, &[], &[]);
    assert_kind_empty("go", "implements", &db);
    // 0.9.7 does not model Go composite literals (`Point{…}` / `&Point{…}`) as
    // constructions, so neither golden nor we emit any `instantiates` for Go.
    assert!(edge_keys(&golden("go"), "instantiates").is_empty());
    assert!(edge_keys(&db, "instantiates").is_empty());

    // Signatures: params-through-result, receiver excluded; assignment tails.
    assert_eq!(
        attr(&db, "Hypot", "geometry.go", "signature").as_deref(),
        Some("(a, b Scalar) Scalar")
    );
    assert_eq!(
        attr(&db, "Point::DistanceTo", "geometry.go", "signature").as_deref(),
        Some("(other Point) Scalar")
    );
    assert_eq!(
        attr(&db, "Unit", "geometry.go", "signature").as_deref(),
        Some("= 1.0")
    );
    assert_eq!(
        attr(&db, "KindCircle", "shapes.go", "signature").as_deref(),
        Some("= iota")
    );
    assert_eq!(attr(&db, "KindRectangle", "shapes.go", "signature"), None);

    // is_exported set on type/func decls (uppercase) but not var/const/method.
    assert_eq!(flag(&db, "Point", "geometry.go", "is_exported"), 1);
    assert_eq!(flag(&db, "Hypot", "geometry.go", "is_exported"), 1);
    assert_eq!(flag(&db, "Unit", "geometry.go", "is_exported"), 0);
    assert_eq!(
        flag(&db, "Point::DistanceTo", "geometry.go", "is_exported"),
        0
    );
    assert_eq!(flag(&db, "KindCircle", "shapes.go", "is_exported"), 0);

    // WHITELIST — Go type-declaration docstrings: 0.9.7 leaves struct/interface/
    // type_alias docstrings NULL (it captures only func/method/var docs). We
    // capture them cleanly (known-better, same class as Python docstrings).
    assert_eq!(
        attr(&golden("go"), "Point", "geometry.go", "docstring"),
        None
    );
    assert_eq!(
        attr(&db, "Point", "geometry.go", "docstring").as_deref(),
        Some("Point is a struct with named fields (struct members).")
    );
    // Func/method/var docstrings match CodeGraph exactly.
    assert_eq!(
        attr(&db, "Hypot", "geometry.go", "docstring").as_deref(),
        attr(&golden("go"), "Hypot", "geometry.go", "docstring").as_deref()
    );
}

// ---- Java -----------------------------------------------------------------

#[test]
fn java_parity() {
    let (_d, db) = build("java");
    assert_core_exact("java", &db);

    // WHITELIST — one reference: `Point other` (a param type) collides by name
    // with the `Point` class's constructor `Point::Point`. CodeGraph 0.9.7
    // resolves the type reference to that *constructor method*; we resolve it to
    // the *class* (more correct). The other reference matches exactly.
    let g_refs = edge_keys(&golden("java"), "references");
    let o_refs = edge_keys(&db, "references");
    let quirk_golden = (
        "fixture::Point::distanceTo".to_string(),
        "fixture::Point::Point".to_string(),
        15,
        29,
    );
    let quirk_ours = (
        "fixture::Point::distanceTo".to_string(),
        "fixture::Point".to_string(),
        15,
        29,
    );
    assert!(
        g_refs.contains(&quirk_golden),
        "golden has the constructor misresolution"
    );
    assert!(
        o_refs.contains(&quirk_ours),
        "we resolve the type reference to the class"
    );
    // Apart from that single edge the two reference sets are identical.
    let mut g_norm = g_refs.clone();
    g_norm.remove(&quirk_golden);
    let mut o_norm = o_refs.clone();
    o_norm.remove(&quirk_ours);
    assert_set_eq("java references (minus quirk)", &o_norm, &g_norm);

    // `extends` (a class's `superclass`: `Dog extends Animal`) and `implements`
    // (its `interfaces`: `Dog implements Measurable`, cross-file) are reproduced
    // exactly. CodeGraph's synthesized interface→impl `calls`
    // (`Measurable::area → Dog::area`, `Animal::weight → Dog::weight`, NULL
    // column) are excluded from the graded `calls` multiset by `edge_keys`.
    assert_edges_wl("java", "extends", &db, &[], &[]);
    assert_edges_wl("java", "implements", &db, &[], &[]);

    // Package → `namespace` node; `::`-qualified names; field & method signatures.
    assert_eq!(
        attr(
            &db,
            "fixture::Geometry::hypot",
            "Geometry.java",
            "signature"
        )
        .as_deref(),
        Some("double (double a, double b)")
    );
    assert_eq!(
        attr(&db, "fixture::Point::Point", "Point.java", "signature").as_deref(),
        Some("(double x, double y)")
    );
    assert_eq!(
        attr(&db, "fixture::Geometry::UNIT", "Geometry.java", "signature").as_deref(),
        Some("double UNIT")
    );
    // Visibility only when explicit; static flag from the modifier.
    assert_eq!(
        attr(&db, "fixture::Geometry", "Geometry.java", "visibility").as_deref(),
        Some("public")
    );
    assert_eq!(
        attr(&db, "fixture::Report", "Shapes.java", "visibility"),
        None
    );
    assert_eq!(
        flag(&db, "fixture::Geometry::UNIT", "Geometry.java", "is_static"),
        1
    );

    // Call diversity (all three resolution paths, matched exactly by
    // `assert_core_exact`): a qualified static call, an unqualified same-class
    // call, and a typed-receiver instance call.
    let calls = edge_keys(&db, "calls");
    assert_eq!(calls.len(), 3);
    let has = |s: &str, t: &str| calls.iter().any(|(cs, ct, ..)| cs == s && ct == t);
    assert!(
        has("fixture::Point::distanceTo", "fixture::Geometry::hypot"),
        "qualified static call Geometry.hypot(...)"
    );
    assert!(
        has("fixture::Report::originGap", "fixture::Report::gap"),
        "unqualified same-class call gap(...)"
    );
    assert!(
        has("fixture::Report::gap", "fixture::Point::distanceTo"),
        "typed-receiver instance call a.distanceTo(...) (a : Point)"
    );

    // `new Point(...)` ×2 → two `instantiates` edges to the class node.
    let insts = edge_keys(&db, "instantiates");
    assert_eq!(insts.len(), 2);
    assert!(insts
        .iter()
        .all(|(s, t, ..)| s == "fixture::Report::originGap" && t == "fixture::Point"));
}
