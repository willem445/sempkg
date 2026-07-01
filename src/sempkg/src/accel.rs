//! Shared CPU/GPU acceleration settings for local llama.cpp inference.
//!
//! The embedder, query expander, and reranker are all in-process GGUF models
//! loaded through llama.cpp. They pick their CPU thread count and GPU offload
//! policy from these helpers so the behaviour — and the `[embedding]`,
//! `[query_expansion]`, and `[reranker]` config knobs — stay consistent across
//! all three.
//!
//! **GPU offload in llama.cpp is a build-time capability.** A plain
//! `cargo build --features embeddings` produces a CPU-only binary; offloading
//! to the GPU requires compiling llama.cpp with a GPU backend, e.g.
//! `cargo build --features embeddings,cuda` (NVIDIA) or `…,vulkan`
//! (vendor-neutral, a good fit for older NVIDIA/AMD cards).
//!
//! [`GpuMode::Auto`] (the default) keeps the *same* config portable: it offloads
//! the whole model to the GPU when the binary has a GPU backend that reports
//! offload support, and transparently runs on the CPU otherwise.

use serde::{Deserialize, Serialize};

/// GPU offload policy for a local inference model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuMode {
    /// Use the GPU when this binary was built with a GPU backend that reports
    /// offload support; otherwise run on the CPU. (default)
    #[default]
    Auto,
    /// Force GPU offload. On a CPU-only build this warns and falls back to CPU.
    On,
    /// CPU only, even on a GPU-capable build.
    Off,
}

impl GpuMode {
    /// Lowercase name used in status output and warnings.
    pub fn as_str(&self) -> &'static str {
        match self {
            GpuMode::Auto => "auto",
            GpuMode::On => "on",
            GpuMode::Off => "off",
        }
    }
}

/// Default CPU thread count for inference: every logical core (at least 1).
pub fn default_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, i32::MAX as usize) as i32
}

/// Resolve a configured thread count: `0` means "all logical cores".
pub fn resolve_threads(n_threads: u32) -> i32 {
    if n_threads == 0 {
        default_threads()
    } else {
        n_threads.min(i32::MAX as u32) as i32
    }
}

/// Human-readable summary of the GPU backend(s) this binary was compiled with,
/// for `status` output. Offload only works when at least one backend is listed.
pub fn gpu_build_status() -> String {
    let mut backends: Vec<&str> = Vec::new();
    if cfg!(feature = "cuda") {
        backends.push("cuda");
    }
    if cfg!(feature = "vulkan") {
        backends.push("vulkan");
    }
    if cfg!(feature = "rocm") {
        backends.push("rocm");
    }
    if cfg!(feature = "metal") {
        backends.push("metal");
    }
    if backends.is_empty() {
        "CPU-only — no GPU backend compiled in (rebuild with e.g. \
         `--features embeddings,cuda` or `…,vulkan`)"
            .to_string()
    } else {
        format!("compiled with {}", backends.join(", "))
    }
}

/// Sentinel layer count meaning "offload the whole model". llama.cpp clamps it
/// down to the model's actual number of layers.
#[cfg(any(feature = "embeddings", feature = "reranker"))]
const ALL_LAYERS: u32 = 1_000_000;

/// Resolve how many transformer layers to offload to the GPU for `section`
/// (used only in warning text, e.g. `"embedding"`).
///
/// * [`GpuMode::Off`] → always `0` (CPU).
/// * an explicit non-zero `gpu_layers` → used verbatim, regardless of mode
///   (advanced manual override — e.g. partial offload on a small GPU).
/// * [`GpuMode::Auto`] → whole model when the backend supports offload, else `0`.
/// * [`GpuMode::On`] → whole model when supported; otherwise warns and returns `0`.
#[cfg(any(feature = "embeddings", feature = "reranker"))]
pub fn resolve_gpu_layers(
    mode: GpuMode,
    gpu_layers: u32,
    backend: &llama_cpp_2::llama_backend::LlamaBackend,
    section: &str,
) -> u32 {
    if mode == GpuMode::Off {
        return 0;
    }
    if gpu_layers > 0 {
        return gpu_layers;
    }
    if backend.supports_gpu_offload() {
        ALL_LAYERS
    } else {
        if mode == GpuMode::On {
            eprintln!(
                "sempkg: warning — [{section}] gpu = \"on\" but this binary was built without a \
                 GPU backend; running on CPU. Rebuild with a GPU backend, e.g. \
                 `cargo build --release --features embeddings,cuda` (or `vulkan`)."
            );
        }
        0
    }
}
