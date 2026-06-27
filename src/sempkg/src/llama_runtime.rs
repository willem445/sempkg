//! Process-wide shared llama.cpp backend.
//!
//! `llama_cpp_2::LlamaBackend` is a process-global singleton guarded by an
//! internal atomic. Its `Drop` calls `llama_backend_free()` and flips that
//! atomic from `true` to `false`; if the atomic is already `false` it hits an
//! `unreachable!()` and panics. Constructing more than one `LlamaBackend` — as
//! we do when the reranker, embedder, and query expander are all loaded — means
//! the second and later `Drop`s crash the process at shutdown (a broken pipe to
//! any client mid-shutdown).
//!
//! The previous workaround (`LlamaBackend {}` for the second init) only moved
//! the problem: every such value still runs the panicking `Drop`. Instead we
//! initialise exactly one backend, keep it in a `OnceLock` for the lifetime of
//! the process, and hand out `&'static` references. The single backend is never
//! dropped (the OS reclaims process memory on exit), so `llama_backend_free()`
//! is never called twice.

use std::sync::OnceLock;

use anyhow::Result;
use llama_cpp_2::llama_backend::LlamaBackend;

static BACKEND: OnceLock<LlamaBackend> = OnceLock::new();

/// Return the process-wide llama.cpp backend, initialising it on first use.
///
/// All llama.cpp model loading and context creation must obtain the backend
/// through this function so that `LlamaBackend::init()` is called at most once.
pub fn shared() -> Result<&'static LlamaBackend> {
    use llama_cpp_2::LlamaCppError;

    if let Some(backend) = BACKEND.get() {
        return Ok(backend);
    }

    match LlamaBackend::init() {
        Ok(backend) => {
            // First writer wins. If another thread set it concurrently, forget
            // our instance rather than dropping it — its `Drop` would free the
            // backend the winner now owns.
            if let Err(extra) = BACKEND.set(backend) {
                std::mem::forget(extra);
            }
        }
        // A backend was created outside this `OnceLock` (should not happen now
        // that every caller routes through here). Fall through to the get below.
        Err(LlamaCppError::BackendAlreadyInitialized) => {}
        Err(e) => return Err(anyhow::anyhow!("llama backend init: {e}")),
    }

    BACKEND
        .get()
        .ok_or_else(|| anyhow::anyhow!("llama backend unavailable after initialization"))
}
