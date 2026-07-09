//! `parity` — quantify semgraph-vs-CodeGraph-0.9.7 graph parity (issue #78,
//! Phase 2 item 6 / "P2c").
//!
//! Given a source tree, this tool indexes it with **semgraph** and obtains the
//! **CodeGraph 0.9.7** side either from a committed/prebuilt golden database
//! (offline/CI mode) or by shelling out to the locally-installed `codegraph`
//! CLI (live/dev mode). It then diffs nodes and edges, applies a committed
//! whitelist of known-better deviations (ADR-003/004), prints per-kind /
//! per-language match percentages plus missing/extra listings, and exits
//! non-zero when the acceptance thresholds are not met — so CI can gate
//! language packs.
//!
//! ```text
//! # Offline (CI) — compare against a committed golden DB:
//! cargo run -p semgraph --bin parity -- tests/fixtures/graph-src \
//!     --golden tests/fixtures/codegraph-v4.db \
//!     --whitelist tests/fixtures/parity-whitelist.json
//!
//! # Live (dev) — build the CodeGraph side by shelling out to codegraph@0.9.7:
//! cargo run -p semgraph --bin parity -- ./src --min-nodes 95 --min-calls 90
//! ```
//!
//! See `docs/parity-harness.md` for the full workflow and for how to add a
//! language to the acceptance flow.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::Parser;
use semgraph::parity::{compare, extract_parity, CompareOptions, ParityReport, Whitelist};
use semgraph::{index_roots, IndexOptions};

/// The CodeGraph version this harness pins its live-mode comparisons to.
const REQUIRED_CODEGRAPH_VERSION: &str = "0.9.7";

#[derive(Parser, Debug)]
#[command(
    name = "parity",
    about = "Quantify semgraph-vs-CodeGraph-0.9.7 graph parity (issue #78 P2c)"
)]
struct Cli {
    /// One or more source roots to index with semgraph and compare.
    #[arg(required = true, value_name = "TREE")]
    roots: Vec<PathBuf>,

    /// Compare against this prebuilt CodeGraph `codegraph.db` (offline mode).
    /// When omitted, the harness runs `codegraph` live (requires version
    /// 0.9.7).
    #[arg(long, value_name = "DB")]
    golden: Option<PathBuf>,

    /// Committed whitelist of known-better deviations (ADR-003/004). Without it,
    /// every delta counts.
    #[arg(long, value_name = "JSON")]
    whitelist: Option<PathBuf>,

    /// Minimum overall node match percentage required to pass.
    #[arg(long, default_value_t = 95.0, value_name = "PCT")]
    min_nodes: f64,

    /// Minimum `calls`-edge match percentage required to pass.
    #[arg(long, default_value_t = 90.0, value_name = "PCT")]
    min_calls: f64,

    /// Pin (start_line, end_line) into the node match key instead of treating
    /// line-range as a whitelistable attribute.
    #[arg(long)]
    strict_line_range: bool,

    /// Print the machine-readable JSON summary to stdout (the human report goes
    /// to stderr).
    #[arg(long)]
    json: bool,

    /// Write the machine-readable JSON summary to this path.
    #[arg(long, value_name = "PATH")]
    json_out: Option<PathBuf>,

    /// In live mode, keep the `.codegraph/` index this run creates instead of
    /// removing it afterwards.
    #[arg(long)]
    keep_codegraph: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(msg) => {
            eprintln!("parity: error: {msg}");
            // Distinguish a harness error (2) from a threshold failure (1).
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> std::result::Result<bool, String> {
    // ---- semgraph side ---------------------------------------------------
    let work = TempWork::new()?;
    let semgraph_db = work.dir.join("semgraph-codegraph.db");
    eprintln!(
        "[parity] indexing {} with semgraph ...",
        cli.roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let stats = index_roots(&cli.roots, &semgraph_db, &IndexOptions::default())
        .map_err(|e| format!("semgraph index failed: {e}"))?;
    eprintln!(
        "[parity]   semgraph: {} files, {} nodes, {} edges",
        stats.file_count, stats.node_count, stats.edge_count
    );

    // ---- CodeGraph side --------------------------------------------------
    let (golden_db, _live_guard) = match &cli.golden {
        Some(db) => {
            if !db.exists() {
                return Err(format!("golden DB not found: {}", db.display()));
            }
            eprintln!("[parity] CodeGraph side: golden DB {}", db.display());
            (db.clone(), None)
        }
        None => {
            eprintln!("[parity] CodeGraph side: live codegraph@{REQUIRED_CODEGRAPH_VERSION}");
            let guard = build_codegraph_live(&cli.roots, cli.keep_codegraph)?;
            (guard.db_path.clone(), Some(guard))
        }
    };

    // ---- Extract + compare ----------------------------------------------
    let ours = extract_parity(&semgraph_db).map_err(|e| format!("reading semgraph DB: {e}"))?;
    let golden = extract_parity(&golden_db).map_err(|e| format!("reading golden DB: {e}"))?;

    let whitelist = match &cli.whitelist {
        Some(p) => Whitelist::load(p).map_err(|e| format!("loading whitelist: {e}"))?,
        None => {
            eprintln!("[parity] no whitelist supplied — every delta counts");
            Whitelist::default()
        }
    };

    let opts = CompareOptions {
        strict_line_range: cli.strict_line_range,
    };
    let report = compare(&ours, &golden, &whitelist, &opts);

    // ---- Output ----------------------------------------------------------
    print_human(&report, cli.min_nodes, cli.min_calls);

    // Augment the JSON with an explicit acceptance verdict so a machine consumer
    // gates on the tool's decision (raw percentages) rather than re-deriving it
    // from the rounded display values.
    let mut json = report.to_json();
    let passed = report.passes(cli.min_nodes, cli.min_calls);
    json["acceptance"] = serde_json::json!({
        "min_nodes": cli.min_nodes,
        "min_calls": cli.min_calls,
        "node_match_pct": report.node_match_pct(),
        "calls_match_pct": report.calls_match_pct(),
        "calls_match_pct_raw": report.calls_pct_raw,
        "passed": passed,
    });
    if let Some(path) = &cli.json_out {
        let text = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        eprintln!("[parity] wrote JSON summary to {}", path.display());
    }
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?
        );
    }

    Ok(passed)
}

/// A scratch directory removed on drop.
struct TempWork {
    dir: PathBuf,
}

impl TempWork {
    fn new() -> std::result::Result<TempWork, String> {
        let dir = std::env::temp_dir().join(format!("semgraph-parity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating temp dir: {e}"))?;
        Ok(TempWork { dir })
    }
}

impl Drop for TempWork {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Handle to a live-built CodeGraph index; removes any `.codegraph/` directories
/// it created on drop (unless `keep` was requested).
struct LiveGuard {
    db_path: PathBuf,
    cleanup: Vec<PathBuf>,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        for dir in &self.cleanup {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Build the CodeGraph side by shelling out to the locally-installed
/// `codegraph` CLI. Verifies the version is exactly 0.9.7 first.
///
/// CodeGraph writes its database to `<root>/.codegraph/codegraph.db`. For a
/// single root we index it and read that file. For multiple roots CodeGraph has
/// no single-database multi-root mode, so live mode supports exactly one root;
/// use `--golden` with a prebuilt DB for multi-root comparisons.
fn build_codegraph_live(roots: &[PathBuf], keep: bool) -> std::result::Result<LiveGuard, String> {
    if roots.len() != 1 {
        return Err(format!(
            "live CodeGraph mode indexes a single root (got {}); \
             build a DB yourself and pass --golden for multi-root comparisons",
            roots.len()
        ));
    }
    let root = &roots[0];
    let exe = which::which("codegraph").map_err(|_| {
        format!(
            "codegraph CLI not found on PATH. Install codegraph@{REQUIRED_CODEGRAPH_VERSION} \
             (`npm install -g @colbymchenry/codegraph@{REQUIRED_CODEGRAPH_VERSION}`) \
             or run offline with --golden <db>."
        )
    })?;

    // Version gate — dev mode requires exactly 0.9.7.
    let version = codegraph_version(&exe)?;
    if version != REQUIRED_CODEGRAPH_VERSION {
        return Err(format!(
            "codegraph version is {version}, but this harness requires \
             {REQUIRED_CODEGRAPH_VERSION}. Install the pinned version \
             (`npm install -g @colbymchenry/codegraph@{REQUIRED_CODEGRAPH_VERSION}`) \
             or run offline with --golden <db>."
        ));
    }
    eprintln!("[parity]   codegraph --version = {version} (ok)");

    let dot_cg = root.join(".codegraph");
    let preexisting = dot_cg.exists();
    let db_path = dot_cg.join("codegraph.db");

    // `init --index` only indexes on first initialization; if a DB already
    // exists, force a full re-index instead (mirrors sembundle's build step).
    let src = root.to_string_lossy().to_string();
    let already = db_path.is_file();
    let args: Vec<&str> = if already {
        vec!["index", "--force", &src]
    } else {
        vec!["init", "--index", &src]
    };
    eprintln!("[parity]   running: codegraph {}", args.join(" "));
    run_codegraph(&exe, &args)?;

    if !db_path.is_file() {
        return Err(format!(
            "codegraph did not produce {} — inspect the output above",
            db_path.display()
        ));
    }

    // Only remove `.codegraph/` if we created it and the caller didn't ask to
    // keep it — never delete a pre-existing index.
    let cleanup = if keep || preexisting {
        Vec::new()
    } else {
        vec![dot_cg]
    };
    Ok(LiveGuard { db_path, cleanup })
}

/// Query `codegraph --version` and return the trimmed version string.
fn codegraph_version(exe: &Path) -> std::result::Result<String, String> {
    let out = build_command(exe, &["--version"])
        .output()
        .map_err(|e| format!("running codegraph --version: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "codegraph --version exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // CodeGraph may print extra text; take the first semver-looking token.
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or_else(|| text.trim())
        .trim()
        .to_string();
    Ok(version)
}

/// Run a codegraph subcommand, streaming its output, and error on failure.
fn run_codegraph(exe: &Path, args: &[&str]) -> std::result::Result<(), String> {
    let status = build_command(exe, args)
        .status()
        .map_err(|e| format!("running codegraph {}: {e}", args.join(" ")))?;
    if !status.success() {
        return Err(format!("codegraph {} failed ({status})", args.join(" ")));
    }
    Ok(())
}

/// Build a `Command`, wrapping `.cmd`/`.bat` shims in `cmd /C` on Windows (the
/// npm-installed `codegraph` is a `.cmd` shim there). Mirrors sembundle's
/// `build_command`.
fn build_command(exe: &Path, args: &[&str]) -> Command {
    #[cfg(windows)]
    {
        let ext = exe
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "cmd" || ext == "bat" {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(exe).args(args);
            return cmd;
        }
    }
    let mut cmd = Command::new(exe);
    cmd.args(args);
    cmd
}

// ---------------------------------------------------------------------------
// Human-readable report
// ---------------------------------------------------------------------------

fn pct_str(p: Option<f64>) -> String {
    match p {
        Some(v) => format!("{v:>6.2}%"),
        None => "   N/A".to_string(),
    }
}

fn print_human(report: &ParityReport, min_nodes: f64, min_calls: f64) {
    let node_pct = report.node_match_pct();
    let calls_pct = report.calls_match_pct();

    eprintln!();
    eprintln!("==================== PARITY REPORT ====================");
    eprintln!(
        "Nodes:  {:>6.2}%   ({}/{} golden identified [{} exact + {} reconvention], \
         {} genuine-missing, {} extra)",
        node_pct,
        report.node_total.matched
            + report.node_total.reconvention
            + report.node_total.whitelisted_missing,
        report.node_total.golden,
        report.node_total.matched,
        report.node_total.reconvention,
        report.node_total.missing - report.node_total.whitelisted_missing,
        report.node_total.extra,
    );
    eprintln!(
        "Edges:  {:>6.2}%   ({}/{} golden matched [convention-normalized], {} extra, \
         {} whitelisted-missing)",
        report.edge_total.match_pct(),
        report.edge_total.matched + report.edge_total.whitelisted_missing,
        report.edge_total.golden,
        report.edge_total.extra,
        report.edge_total.whitelisted_missing,
    );
    eprintln!(
        "calls:  {} normalized  vs  {} raw-qn  (the gap the qn convention hid)",
        pct_str(calls_pct),
        pct_str(report.calls_pct_raw),
    );

    eprintln!("\n-- Nodes by kind --");
    print_groups(&report.nodes_by_kind);
    eprintln!("\n-- Nodes by language --");
    print_groups(&report.nodes_by_language);
    eprintln!("\n-- Edges by kind (convention-normalized) --");
    print_groups(&report.edges_by_kind);
    eprintln!("\n-- Edges by language (convention-normalized) --");
    print_groups(&report.edges_by_language);

    print_diffs(
        "Reconvention nodes (same node, different qn convention — credited to recall)",
        &report.reconvention_nodes,
    );
    print_diffs(
        "Missing nodes (genuinely in CodeGraph, not semgraph)",
        &report.missing_nodes,
    );
    print_diffs(
        "Extra nodes (in semgraph, not CodeGraph)",
        &report.extra_nodes,
    );
    print_diffs(
        "Missing edges (in CodeGraph, not semgraph, after normalization)",
        &report.missing_edges,
    );
    print_diffs(
        "Extra edges (in semgraph, not CodeGraph)",
        &report.extra_edges,
    );
    print_diffs("Node attribute deltas", &report.attr_deltas);

    eprintln!("\n-- Acceptance --");
    let node_ok = node_pct >= min_nodes;
    // A missing calls metric (no golden calls) is vacuously satisfied.
    let calls_ok = calls_pct.is_none_or(|p| p >= min_calls);
    eprintln!(
        "  nodes  {:>6.2}%  >= {:>5.1}%  {}",
        node_pct,
        min_nodes,
        if node_ok { "PASS" } else { "FAIL" }
    );
    eprintln!(
        "  calls  {}  >= {:>5.1}%  {}",
        pct_str(calls_pct),
        min_calls,
        if calls_ok { "PASS" } else { "FAIL" }
    );
    eprintln!(
        "RESULT: {}",
        if node_ok && calls_ok { "PASS" } else { "FAIL" }
    );
    eprintln!("=======================================================");
}

fn print_groups(groups: &[semgraph::parity::GroupStat]) {
    for g in groups {
        eprintln!(
            "  {:<14} {}   golden={:<5} exact={:<5} reconv={:<4} missing={:<4} extra={:<4} wl_missing={}",
            g.label,
            pct_str(g.match_pct_opt()),
            g.golden,
            g.matched,
            g.reconvention,
            g.missing,
            g.extra,
            g.whitelisted_missing,
        );
    }
}

fn print_diffs(title: &str, items: &[semgraph::parity::DiffItem]) {
    let non_wl = items.iter().filter(|d| !d.whitelisted).count();
    let wl = items.len() - non_wl;
    if items.is_empty() {
        return;
    }
    eprintln!("\n-- {title} -- ({non_wl} counted, {wl} whitelisted)");
    for d in items {
        let tag = if d.whitelisted {
            "[whitelisted]"
        } else {
            "[COUNTED]   "
        };
        eprintln!("  {tag} {}", d.description);
        if let Some(j) = &d.justification {
            eprintln!("               ↳ {j}");
        }
    }
}
