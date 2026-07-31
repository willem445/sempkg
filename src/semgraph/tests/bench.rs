//! Manual performance probe (issue #78, Phase 2b) — run explicitly:
//!
//! ```text
//! cargo test -p semgraph --release --test bench -- --ignored --nocapture
//! ```
//!
//! Reports full-index time (now including the Phase 2b resolution pass) and a
//! single-file-change `sync` time over a copy of this repo's `src/` tree, and
//! cross-checks that the incremental sync yields the same node/edge totals as a
//! from-scratch index. Not a CI gate — timings are printed, not asserted.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use semgraph::{index_roots, sync, IndexOptions};

fn repo_src() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/src/semgraph → <repo>/src
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("target") | Some(".git") | Some("node_modules")
        ) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

#[test]
#[ignore = "performance probe; run explicitly with --ignored --nocapture"]
fn bench_index_and_sync_repo_src() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("src");
    copy_tree(&repo_src(), &root);
    let db = dir.path().join("codegraph.db");

    // Full index (parse + resolve).
    let t0 = Instant::now();
    let stats = index_roots(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();
    let full = t0.elapsed();
    println!(
        "FULL INDEX: {} files, {} nodes, {} edges in {:.3}s",
        stats.file_count,
        stats.node_count,
        stats.edge_count,
        full.as_secs_f64()
    );

    // Touch a single source file and time an incremental sync.
    let victim = find_a_rust_file(&root).expect("expected at least one .rs file");
    let mut src = fs::read_to_string(&victim).unwrap();
    src.push_str("\n// benchmark: single-line change to force a re-parse\n");
    fs::write(&victim, src).unwrap();

    let t1 = Instant::now();
    let s2 = sync(std::slice::from_ref(&root), &db, &IndexOptions::default()).unwrap();
    let one = t1.elapsed();
    println!(
        "SINGLE-FILE SYNC: {} files → {} nodes / {} edges in {:.3}s (changed: {})",
        s2.file_count,
        s2.node_count,
        s2.edge_count,
        one.as_secs_f64(),
        victim.strip_prefix(&root).unwrap().display()
    );

    // Correctness cross-check on a large tree: an incremental sync must yield the
    // same persisted totals as a from-scratch index of the same modified tree.
    let scratch = dir.path().join("scratch.db");
    let s3 = index_roots(
        std::slice::from_ref(&root),
        &scratch,
        &IndexOptions::default(),
    )
    .unwrap();
    assert_eq!(
        s2.node_count, s3.node_count,
        "sync node count must equal from-scratch"
    );
    assert_eq!(
        s2.edge_count, s3.edge_count,
        "sync edge count must equal from-scratch"
    );
    println!(
        "SPEEDUP: full {:.3}s vs single-file sync {:.3}s ({:.1}x)",
        full.as_secs_f64(),
        one.as_secs_f64(),
        full.as_secs_f64() / one.as_secs_f64()
    );
}

fn find_a_rust_file(root: &Path) -> Option<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if e.file_name().to_str() != Some("target") {
                    stack.push(p);
                }
            } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                files.push(p);
            }
        }
    }
    files.sort();
    files.into_iter().next()
}
