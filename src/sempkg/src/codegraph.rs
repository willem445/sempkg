/// Codegraph CLI wrapper — scoped to a specific package/bundle directory.
///
/// All queries are strictly scoped: passing a package directory means the
/// operation runs only against that package's index, never cross-package.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::error::SempkgError;

/// Resolved codegraph executable path.
fn codegraph_exe() -> Result<String> {
    which::which("codegraph")
        .or_else(|_| which::which("codegraph.cmd"))
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|_| SempkgError::CodegraphNotFound.into())
}

fn run(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let exe = codegraph_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let out = cmd.output()
        .with_context(|| format!("Failed to run codegraph with args: {args:?}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

    if !out.status.success() {
        return Err(SempkgError::CodegraphError(
            if !stderr.is_empty() { stderr } else { stdout },
        )
        .into());
    }

    Ok(if !stdout.is_empty() { stdout } else { stderr })
}

// ---------------------------------------------------------------------------
// Index management
// ---------------------------------------------------------------------------

/// Run `codegraph init --index <path>` to initialise and index a project.
pub fn init_and_index(path: &Path) -> Result<String> {
    run(&["init", "--index", &path.to_string_lossy()], None)
        .context("codegraph init failed")
}

/// Run `codegraph sync <path>` to incrementally update an existing index.
pub fn sync(path: &Path) -> Result<String> {
    run(&["sync", &path.to_string_lossy()], None)
        .context("codegraph sync failed")
}

/// Run `codegraph status <path>`.
pub fn status(path: &Path) -> Result<String> {
    run(&["status", &path.to_string_lossy()], None)
        .context("codegraph status failed")
}

// ---------------------------------------------------------------------------
// Query operations (all scoped to `project_path`)
// ---------------------------------------------------------------------------

/// Search for symbols by name/pattern.
pub fn query(
    project_path: &Path,
    search: &str,
    kind: Option<&str>,
    limit: usize,
) -> Result<String> {
    let limit_s = limit.to_string();
    let mut args = vec!["query", search, "--json", "--limit", &limit_s];
    let kind_arg;
    if let Some(k) = kind {
        kind_arg = format!("--kind={k}");
        args.push(&kind_arg);
    }
    run(&args, Some(project_path)).context("codegraph query failed")
}

/// Find all callers of a symbol.
pub fn callers(project_path: &Path, symbol: &str, limit: usize) -> Result<String> {
    let limit_s = limit.to_string();
    run(
        &["callers", symbol, "--json", "--limit", &limit_s],
        Some(project_path),
    )
    .context("codegraph callers failed")
}

/// Find all callees of a symbol.
pub fn callees(project_path: &Path, symbol: &str, limit: usize) -> Result<String> {
    let limit_s = limit.to_string();
    run(
        &["callees", symbol, "--json", "--limit", &limit_s],
        Some(project_path),
    )
    .context("codegraph callees failed")
}

/// Get AI-optimised context for a natural-language task description.
pub fn context(project_path: &Path, task: &str) -> Result<String> {
    run(&["context", task], Some(project_path)).context("codegraph context failed")
}

/// Analyse the impact (downstream dependents) of changing a symbol.
pub fn impact(project_path: &Path, symbol: &str, depth: usize) -> Result<String> {
    let depth_s = depth.to_string();
    run(
        &["impact", symbol, "--json", "--depth", &depth_s],
        Some(project_path),
    )
    .context("codegraph impact failed")
}

/// List files tracked by the index.
pub fn files(project_path: &Path, filter: Option<&str>) -> Result<String> {
    let mut args = vec!["files", "--json"];
    if let Some(f) = filter {
        args.extend(["--filter", f]);
    }
    run(&args, Some(project_path)).context("codegraph files failed")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the path to the codegraph SQLite database for a project.
pub fn db_path(project_path: &Path) -> PathBuf {
    project_path.join(".codegraph").join("codegraph.db")
}

/// Return true if the project has an existing codegraph index.
pub fn is_indexed(project_path: &Path) -> bool {
    db_path(project_path).exists()
}
