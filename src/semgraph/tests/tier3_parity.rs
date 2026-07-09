//! Tier-3 language-pack parity vs CodeGraph 0.9.7 goldens (issue #78 Phase 2c
//! part 3): Ruby, PHP, Kotlin, Swift, Scala, C#.
//!
//! For each language, a native `index_roots` of `tests/fixtures/graph-src-tier3/
//! <lang>/` must reproduce the committed golden `tests/fixtures/graph-src-tier3/
//! <lang>.db` (produced by `codegraph init --index` with 0.9.7):
//!
//! - **nodes** — the `(kind, qualified_name, file_path)` keyset, ≥95% of golden
//!   (we hold 100% on the fixtures).
//! - **`calls` / `instantiates` / `imports` edges** — each graded as a
//!   bidirectional `(source_qn, target_qn, line, col)` multiset (no missing, no
//!   spurious), ≥90% of golden (100% on the fixtures), minus a small, documented
//!   per-language whitelist (see `docs/arch/adr/adr-005-tier3-language-packs.md`).
//!
//! Whitelisted deltas, all disclosed in ADR-005:
//! - **Synthesized interface→impl `calls`** (Kotlin/Scala/C#): CodeGraph's
//!   Phase-5.5 heuristic (`metadata.synthesizedBy = "interface-impl"`) carries a
//!   NULL call-site column; excluded from the graded `calls` multiset globally.
//! - **Kotlin `import` target**: CodeGraph resolves `import a.b.C` to the imported
//!   class node; we point the `imports` edge at our own `import` node. One
//!   whitelisted missing+spurious pair (Kotlin only).
//!
//! Not graded here (ungraded edge families, per ADR-005): `implements`/`extends`
//! and `references`. Scala emits **no** `instantiates` (0.9.7 has no Scala
//! instantiation handling); the fixture's `new Circle` is present to prove we
//! likewise emit none — graded as an exact empty multiset.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use semgraph::{index_roots, IndexOptions};

/// A per-kind whitelist: golden edges we do not reproduce (`missing`) and edges
/// we emit that the golden lacks (`spurious`), keyed by `(source_qn, target_qn,
/// line)` (column-agnostic). Empty unless a delta is documented in ADR-005.
struct Wl {
    missing: &'static [(&'static str, &'static str, i64)],
    spurious: &'static [(&'static str, &'static str, i64)],
}

const EMPTY: Wl = Wl {
    missing: &[],
    spurious: &[],
};

struct Case {
    lang: &'static str,
    calls: Wl,
    instantiates: Wl,
    imports: Wl,
    /// Golden node keys `(kind, qualified_name)` we deliberately do not
    /// reproduce. Empty for every current fixture.
    node_whitelist: &'static [(&'static str, &'static str)],
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            lang: "ruby",
            calls: EMPTY,
            instantiates: EMPTY,
            imports: EMPTY,
            node_whitelist: &[],
        },
        Case {
            lang: "php",
            calls: EMPTY,
            instantiates: EMPTY,
            imports: EMPTY,
            node_whitelist: &[],
        },
        Case {
            lang: "kotlin",
            calls: EMPTY,
            instantiates: EMPTY,
            // CodeGraph resolves `import com.example.geo.Circle` to the Circle
            // class node; we point at our own import node. Disclosed, ungraded.
            imports: Wl {
                missing: &[("com.example.app", "com.example.geo::Circle", 3)],
                spurious: &[(
                    "com.example.app",
                    "com.example.app::com.example.geo.Circle",
                    3,
                )],
            },
            node_whitelist: &[],
        },
        Case {
            lang: "swift",
            calls: EMPTY,
            instantiates: EMPTY,
            imports: EMPTY,
            node_whitelist: &[],
        },
        Case {
            lang: "scala",
            calls: EMPTY,
            instantiates: EMPTY,
            imports: EMPTY,
            node_whitelist: &[],
        },
        Case {
            lang: "csharp",
            calls: EMPTY,
            instantiates: EMPTY,
            imports: EMPTY,
            node_whitelist: &[],
        },
    ]
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/graph-src-tier3")
}

type NodeKey = (String, String, String);
/// `(source_qn, target_qn, line, col)`.
type EdgeKey = (String, String, i64, i64);

fn node_keyset(db: &Path) -> BTreeSet<NodeKey> {
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

/// Edges of `kind` keyed by `(source_qn, target_qn, line, col)`. For `calls`,
/// rows with a NULL column — CodeGraph's synthesized interface→impl edges — are
/// excluded from the graded multiset.
fn edge_keys(db: &Path, kind: &str) -> Vec<EdgeKey> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let extra = if kind == "calls" {
        " AND e.line IS NOT NULL AND e.col IS NOT NULL"
    } else {
        ""
    };
    let sql = format!(
        "SELECT s.qualified_name, t.qualified_name, COALESCE(e.line, -1), COALESCE(e.col, -1) \
         FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target \
         WHERE e.kind = ?1{extra}"
    );
    let mut stmt = conn.prepare(&sql).unwrap();
    let mut v: Vec<EdgeKey> = stmt
        .query_map([kind], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    v.sort();
    v
}

fn wl_hit(wl: &[(&str, &str, i64)], k: &EdgeKey) -> bool {
    wl.iter()
        .any(|(s, t, l)| *s == k.0 && *t == k.1 && *l == k.2)
}

/// Grade one edge kind bidirectionally against the golden, honouring the
/// whitelist. Returns `(reproduced, total, missing, spurious)`.
fn grade_edges(
    lang: &str,
    kind: &str,
    our_db: &Path,
    golden_db: &Path,
    wl: &Wl,
    failures: &mut Vec<String>,
) {
    let ours: Vec<EdgeKey> = edge_keys(our_db, kind)
        .into_iter()
        .filter(|k| !wl_hit(wl.spurious, k))
        .collect();
    let golden: Vec<EdgeKey> = edge_keys(golden_db, kind)
        .into_iter()
        .filter(|k| !wl_hit(wl.missing, k))
        .collect();
    let ours_set: BTreeSet<&EdgeKey> = ours.iter().collect();
    let golden_set: BTreeSet<&EdgeKey> = golden.iter().collect();
    let missing: Vec<&EdgeKey> = golden.iter().filter(|k| !ours_set.contains(k)).collect();
    let spurious: Vec<&EdgeKey> = ours.iter().filter(|k| !golden_set.contains(k)).collect();
    let reproduced = golden.len() - missing.len();
    let pct = if golden.is_empty() {
        100.0
    } else {
        100.0 * reproduced as f64 / golden.len() as f64
    };
    eprintln!(
        "[{lang}] {kind}: {reproduced}/{} golden reproduced ({pct:.1}%), {} spurious",
        golden.len(),
        spurious.len()
    );
    if !missing.is_empty() {
        failures.push(format!("{lang}: missing golden {kind}: {missing:#?}"));
    }
    if !spurious.is_empty() {
        failures.push(format!("{lang}: spurious {kind}: {spurious:#?}"));
    }
}

fn build(dir: &Path) -> tempfile::TempDir {
    let out = tempfile::TempDir::new().unwrap();
    let db = out.path().join("codegraph.db");
    index_roots(&[dir.to_path_buf()], &db, &IndexOptions::default()).unwrap();
    out
}

#[test]
fn tier3_parity_meets_acceptance() {
    let mut failures = Vec::new();
    let mut ran = 0;

    for case in cases() {
        let src = fixtures_root().join(case.lang);
        let golden = fixtures_root().join(format!("{}.db", case.lang));
        if !golden.exists() || !src.exists() {
            eprintln!("skip {}: fixture/golden not present yet", case.lang);
            continue;
        }
        ran += 1;

        let tmp = build(&src);
        let our_db = tmp.path().join("codegraph.db");

        // ---- nodes ----
        let ours = node_keyset(&our_db);
        let golden_nodes = node_keyset(&golden);
        let wl: BTreeSet<(&str, &str)> = case.node_whitelist.iter().copied().collect();
        let golden_wanted: BTreeSet<&NodeKey> = golden_nodes
            .iter()
            .filter(|(k, qn, _)| !wl.contains(&(k.as_str(), qn.as_str())))
            .collect();
        let missing: Vec<&&NodeKey> = golden_wanted
            .iter()
            .filter(|k| !ours.contains(**k))
            .collect();
        let extra: Vec<&NodeKey> = ours.iter().filter(|k| !golden_nodes.contains(*k)).collect();
        let reproduced = golden_wanted.len() - missing.len();
        let pct = if golden_wanted.is_empty() {
            100.0
        } else {
            100.0 * reproduced as f64 / golden_wanted.len() as f64
        };
        eprintln!(
            "[{}] nodes: {}/{} golden reproduced ({:.1}%), {} extra",
            case.lang,
            reproduced,
            golden_wanted.len(),
            pct,
            extra.len()
        );
        if !missing.is_empty() {
            failures.push(format!(
                "{}: missing golden nodes: {:#?}",
                case.lang, missing
            ));
        }
        if !extra.is_empty() {
            failures.push(format!("{}: spurious nodes: {:#?}", case.lang, extra));
        }

        // ---- edges: calls, instantiates, imports (all bidirectional) ----
        grade_edges(
            case.lang,
            "calls",
            &our_db,
            &golden,
            &case.calls,
            &mut failures,
        );
        grade_edges(
            case.lang,
            "instantiates",
            &our_db,
            &golden,
            &case.instantiates,
            &mut failures,
        );
        grade_edges(
            case.lang,
            "imports",
            &our_db,
            &golden,
            &case.imports,
            &mut failures,
        );
    }

    assert!(ran > 0, "no tier-3 fixtures found");
    assert!(
        failures.is_empty(),
        "tier-3 parity failures:\n{}",
        failures.join("\n")
    );
}
