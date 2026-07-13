//! `sempkg status` — diagnostic report for this installation.
//!
//! Bare `sempkg status` answers the questions every bug report needs: which
//! version and build features this binary has, whether it can offload to a GPU,
//! where the GGUF models are and whether they are on disk, what the workspace
//! and global stores contain, and whether the CodeGraph CLI is reachable.
//!
//! `sempkg status <name>` is a different question (the state of one bundle /
//! package) and stays in `main.rs` unchanged.
//!
//! This is a leaf module: it only reads through existing public helpers
//! (`accel`, `codegraph`, `embedding`, `manifest`, `packages`, `query_expansion`,
//! `reranker`, `store`) and never mutates state.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::accel;
use crate::codegraph;
use crate::embedding;
use crate::manifest;
use crate::packages::PackageRegistry;
use crate::providers::ProviderKind;
use crate::query_expansion;
use crate::reranker;
use crate::store::{global_store_dir, BundleStore};

/// Every optional cargo feature that changes what the binary can do at runtime,
/// paired with whether it was compiled in.
fn build_features() -> Vec<String> {
    [
        ("reranker", cfg!(feature = "reranker")),
        ("embeddings", cfg!(feature = "embeddings")),
        ("cuda", cfg!(feature = "cuda")),
        ("vulkan", cfg!(feature = "vulkan")),
        ("rocm", cfg!(feature = "rocm")),
        ("metal", cfg!(feature = "metal")),
    ]
    .into_iter()
    .filter(|(_, enabled)| *enabled)
    .map(|(name, _)| name.to_string())
    .collect()
}

/// The git commit this binary was built from.
///
/// Release builds run in GitHub Actions, where `GITHUB_SHA` is set, so the
/// published artifacts carry their commit. Local `cargo build` / `cargo install`
/// binaries report `unknown` rather than growing a build script for it.
fn build_commit() -> String {
    option_env!("GITHUB_SHA").unwrap_or("unknown").to_string()
}

fn provider_label(provider: &ProviderKind) -> String {
    match provider {
        ProviderKind::Local => "local",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Copilot => "copilot",
    }
    .to_string()
}

/// Status of one local-inference section (`[embedding]`, `[reranker]`,
/// `[query_expansion]`).
#[derive(Debug, Serialize)]
pub struct ModelStatus {
    pub enabled: bool,
    pub provider: String,
    /// Model identifier, where the section has a choice of models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub gpu: String,
    pub gpu_layers: u32,
    pub cpu_threads: i32,
    /// Resolved GGUF path. Meaningful only for `provider = "local"`.
    pub model_path: String,
    pub model_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_size_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    pub manifest_present: bool,
    pub lock_present: bool,
    pub bundles: usize,
}

#[derive(Debug, Serialize)]
pub struct ModelFile {
    pub name: String,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct GlobalStatus {
    pub directory: String,
    pub exists: bool,
    pub bundles: usize,
    pub models: Vec<ModelFile>,
    pub local_packages: usize,
}

#[derive(Debug, Serialize)]
pub struct CodegraphStatus {
    pub on_path: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub version: String,
}

/// Everything `sempkg status` reports, in one serializable value.
#[derive(Debug, Serialize)]
pub struct DiagnosticReport {
    pub version: String,
    pub commit: String,
    pub os: String,
    pub arch: String,
    pub features: Vec<String>,
    pub gpu_build: String,
    pub cpu_threads: i32,
    pub embedding: ModelStatus,
    pub reranker: ModelStatus,
    pub query_expansion: ModelStatus,
    pub workspace: WorkspaceStatus,
    pub global: GlobalStatus,
    pub codegraph: CodegraphStatus,
}

/// Describe a model file on disk without failing when it is absent — a missing
/// model is a normal state (`sempkg embedding pull` has not been run yet), and
/// reporting it is the whole point of this command.
fn model_file_status(path: &Path) -> (bool, Option<u64>) {
    match std::fs::metadata(path) {
        Ok(md) if md.is_file() => (true, Some(md.len())),
        _ => (false, None),
    }
}

fn embedding_status(cfg: &embedding::EmbeddingConfig) -> ModelStatus {
    let path = cfg.resolved_model_path();
    let (present, size) = model_file_status(&path);
    ModelStatus {
        enabled: cfg.enabled,
        provider: provider_label(&cfg.provider),
        model_id: Some(match cfg.model() {
            Ok(m) => format!("{} (dim {})", m.id(), m.dim()),
            Err(e) => format!("<invalid> — {e}"),
        }),
        gpu: cfg.gpu.as_str().to_string(),
        gpu_layers: cfg.gpu_layers,
        cpu_threads: accel::resolve_threads(cfg.n_threads),
        model_path: path.display().to_string(),
        model_present: present,
        model_size_bytes: size,
    }
}

fn reranker_status(cfg: &reranker::RerankerConfig) -> ModelStatus {
    let path = cfg.resolved_model_path();
    let (present, size) = model_file_status(&path);
    ModelStatus {
        enabled: cfg.enabled,
        provider: provider_label(&cfg.provider),
        model_id: None,
        gpu: cfg.gpu.as_str().to_string(),
        gpu_layers: cfg.gpu_layers,
        cpu_threads: accel::resolve_threads(cfg.n_threads),
        model_path: path.display().to_string(),
        model_present: present,
        model_size_bytes: size,
    }
}

fn query_expansion_status(cfg: &query_expansion::QueryExpansionConfig) -> ModelStatus {
    let path = cfg.resolved_model_path();
    let (present, size) = model_file_status(&path);
    ModelStatus {
        enabled: cfg.enabled,
        provider: provider_label(&cfg.provider),
        model_id: None,
        gpu: cfg.gpu.as_str().to_string(),
        gpu_layers: cfg.gpu_layers,
        cpu_threads: accel::resolve_threads(cfg.n_threads),
        model_path: path.display().to_string(),
        model_present: present,
        model_size_bytes: size,
    }
}

fn workspace_status(workspace: Option<&Path>) -> WorkspaceStatus {
    let Some(dir) = workspace else {
        return WorkspaceStatus {
            directory: None,
            manifest_present: false,
            lock_present: false,
            bundles: 0,
        };
    };

    WorkspaceStatus {
        directory: Some(dir.display().to_string()),
        manifest_present: dir.join(manifest::MANIFEST_FILE).is_file(),
        lock_present: dir.join(manifest::LOCK_FILE).is_file(),
        bundles: BundleStore::workspace(dir).list().len(),
    }
}

fn global_status() -> GlobalStatus {
    // `global_store_dir()` is ~/.sempkg/bundles — the data root is its parent.
    let bundles_dir = global_store_dir();
    let root = bundles_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(bundles_dir);

    let mut models: Vec<ModelFile> = std::fs::read_dir(embedding::default_model_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let md = entry.metadata().ok()?;
            if !md.is_file() {
                return None;
            }
            Some(ModelFile {
                name: entry.file_name().to_string_lossy().to_string(),
                size_bytes: md.len(),
            })
        })
        .collect();
    models.sort_by(|a, b| a.name.cmp(&b.name));

    GlobalStatus {
        directory: root.display().to_string(),
        exists: root.is_dir(),
        bundles: BundleStore::global().list().len(),
        models,
        local_packages: PackageRegistry::load()
            .map(|reg| reg.list().len())
            .unwrap_or(0),
    }
}

fn codegraph_status() -> CodegraphStatus {
    let path = codegraph::exe_on_path();
    CodegraphStatus {
        on_path: path.is_some(),
        version: if path.is_some() {
            codegraph::version()
        } else {
            "unknown".to_string()
        },
        path,
    }
}

/// Collect the full diagnostic report. Never fails: an unreadable workspace,
/// a missing model, or an absent CodeGraph CLI are all *findings*, not errors.
pub fn gather(workspace: Option<&Path>) -> DiagnosticReport {
    // Each section falls back to its defaults when the workspace has no
    // manifest — exactly what the runtime does, so the report describes the
    // config the models would actually load.
    let manifest = workspace.and_then(|d| manifest::load_manifest(d).ok());
    let (embedding_cfg, reranker_cfg, query_expansion_cfg) = match manifest {
        Some(mf) => (
            mf.embedding.unwrap_or_default(),
            mf.reranker.unwrap_or_default(),
            mf.query_expansion.unwrap_or_default(),
        ),
        None => (
            embedding::EmbeddingConfig::default(),
            reranker::RerankerConfig::default(),
            query_expansion::QueryExpansionConfig::default(),
        ),
    };

    DiagnosticReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit: build_commit(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        features: build_features(),
        gpu_build: accel::gpu_build_status(),
        cpu_threads: accel::default_threads(),
        embedding: embedding_status(&embedding_cfg),
        reranker: reranker_status(&reranker_cfg),
        query_expansion: query_expansion_status(&query_expansion_cfg),
        workspace: workspace_status(workspace),
        global: global_status(),
        codegraph: codegraph_status(),
    }
}

/// Render a byte count the way a human reads a model file size.
fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.1} MB", b / MB)
    }
}

fn print_model_section(title: &str, m: &ModelStatus) {
    println!();
    println!("[{title}]");
    println!("  enabled     : {}", m.enabled);
    println!("  provider    : {}", m.provider);
    if let Some(id) = &m.model_id {
        println!("  model       : {id}");
    }
    println!(
        "  gpu         : {}{}",
        m.gpu,
        if m.gpu_layers > 0 {
            format!(" (manual override: {} layers)", m.gpu_layers)
        } else {
            String::new()
        }
    );
    println!("  cpu threads : {}", m.cpu_threads);
    println!("  model file  : {}", m.model_path);
    println!(
        "  model state : {}",
        match m.model_size_bytes {
            Some(size) => format!("✓ present ({})", human_size(size)),
            None => "✗ missing".to_string(),
        }
    );
}

fn print_report(r: &DiagnosticReport) {
    println!("sempkg {}", r.version);
    println!("  commit      : {}", r.commit);
    println!("  os / arch   : {} / {}", r.os, r.arch);
    println!(
        "  features    : {}",
        if r.features.is_empty() {
            "(none)".to_string()
        } else {
            r.features.join(", ")
        }
    );
    println!("  gpu build   : {}", r.gpu_build);
    println!("  cpu threads : {}", r.cpu_threads);

    print_model_section("embedding", &r.embedding);
    print_model_section("reranker", &r.reranker);
    print_model_section("query_expansion", &r.query_expansion);

    println!();
    println!("[workspace]");
    match &r.workspace.directory {
        Some(dir) => println!("  directory   : {dir}"),
        None => println!("  directory   : (none detected)"),
    }
    println!(
        "  sempkg.toml : {}",
        if r.workspace.manifest_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "  sempkg.lock : {}",
        if r.workspace.lock_present {
            "present"
        } else {
            "missing"
        }
    );
    println!("  bundles     : {}", r.workspace.bundles);

    println!();
    println!("[global]");
    println!(
        "  directory   : {} ({})",
        r.global.directory,
        if r.global.exists {
            "present"
        } else {
            "missing"
        }
    );
    println!("  bundles     : {}", r.global.bundles);
    println!("  packages    : {}", r.global.local_packages);
    if r.global.models.is_empty() {
        println!("  models      : (none downloaded)");
    } else {
        println!("  models      :");
        for m in &r.global.models {
            println!("    {} ({})", m.name, human_size(m.size_bytes));
        }
    }

    println!();
    println!("[codegraph]");
    match &r.codegraph.path {
        Some(p) => println!("  on PATH     : yes ({p})"),
        None => {
            println!("  on PATH     : no — install with `npm install -g @colbymchenry/codegraph`")
        }
    }
    println!("  version     : {}", r.codegraph.version);
}

/// Run bare `sempkg status`: print the diagnostic report as text or JSON.
pub fn run(workspace: Option<&Path>, json: bool) -> Result<()> {
    let report = gather(workspace);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What this pins, and what it cannot.
    ///
    /// `reranker` and `embeddings` are checked in **both** directions, and CI
    /// exercises both: the plain `cargo test` run compiles with neither, the
    /// `--features reranker,embeddings` run with both. So a dropped, misnamed,
    /// or unconditionally-pushed entry for those two fails here.
    ///
    /// The GPU rows (cuda/vulkan/rocm/metal) can only be exercised *negatively*.
    /// No test job builds a GPU backend — each needs a vendor SDK, and
    /// `--all-features` is unbuildable because those SDKs conflict — so their
    /// `cfg!` is false in every test binary. The assertion below therefore pins
    /// only that a GPU backend is never *claimed* by a build that lacks it
    /// (reporting `cuda` on a CPU build would be a lie a bug report acts on),
    /// and cannot catch a GPU row wrongly omitted from the list. That direction
    /// is covered outside the test suite: the release GPU artifacts are built
    /// with these features, and `gpu build` in the same report is derived
    /// independently by `accel::gpu_build_status()`, so a missing row shows up
    /// as a report that claims a GPU backend while listing no GPU feature.
    #[test]
    fn features_match_compiled_cfg() {
        const KNOWN: [&str; 6] = ["reranker", "embeddings", "cuda", "vulkan", "rocm", "metal"];

        let features = build_features();

        assert_eq!(
            features.iter().any(|f| f == "reranker"),
            cfg!(feature = "reranker"),
            "reported features must match the cargo features this binary was built with"
        );
        assert_eq!(
            features.iter().any(|f| f == "embeddings"),
            cfg!(feature = "embeddings")
        );

        for f in &features {
            assert!(KNOWN.contains(&f.as_str()), "unknown feature reported: {f}");
        }
        assert_eq!(
            features.len(),
            features
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            "features must not repeat: {features:?}"
        );

        // Negative direction only — see the doc comment above.
        for (name, enabled) in [
            ("cuda", cfg!(feature = "cuda")),
            ("vulkan", cfg!(feature = "vulkan")),
            ("rocm", cfg!(feature = "rocm")),
            ("metal", cfg!(feature = "metal")),
        ] {
            if !enabled {
                assert!(
                    !features.iter().any(|f| f == name),
                    "`{name}` reported on a build compiled without it"
                );
            }
        }
    }

    #[test]
    fn report_serializes_expected_keys() {
        let report = gather(None);
        let value = serde_json::to_value(&report).expect("report should serialize");
        let obj = value.as_object().expect("report should be a JSON object");

        for key in [
            "version",
            "commit",
            "os",
            "arch",
            "features",
            "gpu_build",
            "cpu_threads",
            "embedding",
            "reranker",
            "query_expansion",
            "workspace",
            "global",
            "codegraph",
        ] {
            assert!(obj.contains_key(key), "report is missing `{key}`");
        }

        assert_eq!(obj["version"], env!("CARGO_PKG_VERSION"));
        assert!(obj["embedding"]["model_path"].is_string());
        assert!(obj["gpu_build"].is_string());
    }

    /// A workspace that does not exist is a finding, not a failure: the report
    /// must still be produced with the absent pieces marked missing.
    #[test]
    fn missing_workspace_reports_absent_fields_without_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-workspace");

        let report = gather(Some(&missing));

        assert_eq!(
            report.workspace.directory,
            Some(missing.display().to_string())
        );
        assert!(!report.workspace.manifest_present);
        assert!(!report.workspace.lock_present);
        assert_eq!(report.workspace.bundles, 0);
        // Config falls back to defaults, so the model paths are still reported.
        assert!(!report.embedding.model_path.is_empty());
    }

    #[test]
    fn human_size_switches_unit_at_a_gigabyte() {
        assert_eq!(human_size(512 * 1024 * 1024), "512.0 MB");
        assert_eq!(human_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }
}
