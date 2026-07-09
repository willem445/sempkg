//! Tier-3 language-pack parity vs CodeGraph 0.9.7 goldens (issue #78 Phase 2c
//! part 3): Ruby, PHP, Kotlin, Swift, Scala, C#.
//!
//! For each language, a native `index_roots` of `tests/fixtures/graph-src-tier3/
//! <lang>/` must reproduce the committed golden `tests/fixtures/graph-src-tier3/
//! <lang>.db` (produced by `codegraph init --index` with 0.9.7):
//!
//! - **nodes** — the `(kind, qualified_name, file_path)` keyset, ≥95% of golden
//!   (we hold 100% on the fixtures).
//! - **`calls` edges** — the `(source_qn, target_qn, line, col)` multiset, ≥90%
//!   of golden (100% on the fixtures), excluding a small, documented per-language
//!   whitelist of known-better/known-different 0.9.7 emissions (see
//!   `docs/arch/adr/adr-005-tier3-language-packs.md`).
//!
//! The whitelist is deliberately tiny and each entry is justified in the ADR.
//! Golden `calls` with a NULL line are CodeGraph's *synthesized* interface→impl
//! edges (its Phase-5.5 heuristic, `metadata.synthesizedBy = "interface-impl"`),
//! an advanced feature the native indexer does not replicate; those are excluded
//! from the graded `calls` multiset for every language.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use semgraph::{index_roots, IndexOptions};

struct Case {
    lang: &'static str,
    /// Golden `calls` keys we deliberately do not reproduce, as
    /// `(source_qn, target_qn, line)` — beyond the global NULL-line synthesized
    /// exclusion. Empty for every current fixture.
    calls_whitelist: &'static [(&'static str, &'static str, i64)],
    /// Golden node keys `(kind, qualified_name)` we deliberately do not
    /// reproduce. Empty for every current fixture.
    node_whitelist: &'static [(&'static str, &'static str)],
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            lang: "ruby",
            calls_whitelist: &[],
            node_whitelist: &[],
        },
        Case {
            lang: "php",
            calls_whitelist: &[],
            node_whitelist: &[],
        },
        Case {
            lang: "kotlin",
            calls_whitelist: &[],
            node_whitelist: &[],
        },
        Case {
            lang: "swift",
            calls_whitelist: &[],
            node_whitelist: &[],
        },
        Case {
            lang: "scala",
            calls_whitelist: &[],
            node_whitelist: &[],
        },
        Case {
            lang: "csharp",
            calls_whitelist: &[],
            node_whitelist: &[],
        },
    ]
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/graph-src-tier3")
}

type NodeKey = (String, String, String);
type CallKey = (String, String, i64, i64);

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

/// `calls` edges keyed by `(source_qn, target_qn, line, col)`. Golden rows with
/// a NULL line (synthesized interface-impl edges) are excluded.
fn call_keys(db: &Path) -> Vec<CallKey> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            // Synthesized interface→impl `calls` (CodeGraph's Phase-5.5 heuristic)
            // carry a NULL call-site column; exclude them from the graded multiset.
            "SELECT s.qualified_name, t.qualified_name, e.line, e.col \
             FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target \
             WHERE e.kind = 'calls' AND e.line IS NOT NULL AND e.col IS NOT NULL",
        )
        .unwrap();
    let mut v: Vec<CallKey> = stmt
        .query_map([], |r| {
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

        // ---- calls ----
        let ours_calls = call_keys(&our_db);
        let golden_calls: Vec<CallKey> = call_keys(&golden)
            .into_iter()
            .filter(|(s, t, l, _)| {
                !case
                    .calls_whitelist
                    .iter()
                    .any(|(ws, wt, wl)| ws == s && wt == t && wl == l)
            })
            .collect();
        let ours_set: BTreeSet<&CallKey> = ours_calls.iter().collect();
        let cmissing: Vec<&CallKey> = golden_calls
            .iter()
            .filter(|k| !ours_set.contains(k))
            .collect();
        let golden_set: BTreeSet<&CallKey> = golden_calls.iter().collect();
        let cextra: Vec<&CallKey> = ours_calls
            .iter()
            .filter(|k| !golden_set.contains(k))
            .collect();
        let creprod = golden_calls.len() - cmissing.len();
        let cpct = if golden_calls.is_empty() {
            100.0
        } else {
            100.0 * creprod as f64 / golden_calls.len() as f64
        };
        eprintln!(
            "[{}] calls: {}/{} golden reproduced ({:.1}%), {} extra",
            case.lang,
            creprod,
            golden_calls.len(),
            cpct,
            cextra.len()
        );
        if !cmissing.is_empty() {
            failures.push(format!(
                "{}: missing golden calls: {:#?}",
                case.lang, cmissing
            ));
        }
        if !cextra.is_empty() {
            failures.push(format!("{}: spurious calls: {:#?}", case.lang, cextra));
        }
    }

    assert!(ran > 0, "no tier-3 fixtures found");
    assert!(
        failures.is_empty(),
        "tier-3 parity failures:\n{}",
        failures.join("\n")
    );
}
