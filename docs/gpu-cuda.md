# GPU acceleration (NVIDIA CUDA)

sempkg runs its embedding, query-expansion, and reranker models in-process
through llama.cpp. GPU offload is a **build-time** capability: the standard
release binaries are CPU-only, and a separate CUDA build offloads these models
to an NVIDIA GPU.

The release workflow publishes CUDA artifacts alongside the CPU binaries:

| Artifact | Contents |
| --- | --- |
| `sempkg-x86_64-pc-windows-msvc-cuda.zip` | `sempkg.exe` + bundled CUDA runtime DLLs |
| `sempkg-x86_64-unknown-linux-gnu-cuda` | self-contained binary (CUDA runtime linked statically) |

## Installing

The `install.sh` / `install.ps1` scripts pick the build automatically: if a
supported NVIDIA GPU (compute capability ≥ 7.5) with a driver ≥ 580 is detected
via `nvidia-smi`, they install the GPU build; otherwise they fall back to the
CPU build. Override the choice with `--gpu on|off` (`-Gpu on|off` on Windows) —
for example `--gpu off` to force the CPU build. GPU selection applies to
`sempkg` only; `sembundle` has no GPU variant.

## What you need to run it

You do **not** need to install the CUDA Toolkit or recompile — the CUDA runtime
libraries are bundled (Windows) or statically linked (Linux).

You **do** need a recent NVIDIA driver. The GPU-facing driver library
(`nvcuda.dll` on Windows, `libcuda.so` on Linux) ships with the driver and
cannot legally be redistributed, so it is never bundled.

- **Driver version:** the build targets CUDA 13.x. Thanks to CUDA minor-version
  compatibility, any driver from the CUDA 13.0 era or newer works — roughly
  **≥ 580 (Windows and Linux)**.
- **GPU:** Turing (compute capability 7.5) or newer — i.e. GeForce RTX 20xx and
  up, plus the equivalent Quadro/data-center parts. CUDA 13 dropped
  Maxwell, Pascal, and Volta, so GTX 9xx/10xx and Titan V cards are **not**
  supported by this build; use the `vulkan` backend for those (see below).
  Architectures without pre-built SASS (anything past RTX 40xx) are covered by
  JIT-compiled PTX, so a short one-time compile pause may occur on first run.

### Windows

Unzip the archive and keep `sempkg.exe` next to the bundled DLLs
(`cudart64_*.dll`, `cublas64_*.dll`, `cublasLt64_*.dll`) — Windows loads them
from the executable's own directory. Moving the `.exe` out on its own will make
it fall back to any CUDA install on `PATH`, or fail to start if there is none.

### Linux

The binary is self-contained. Ensure the NVIDIA driver is installed so
`libcuda.so.1` is present on the loader path.

## Verifying GPU offload

`gpu = "auto"` (the default in the `[embedding]`, `[query_expansion]`, and
`[reranker]` config sections) offloads to the GPU automatically when the binary
has a working GPU backend, and silently falls back to CPU otherwise. `sempkg
status` reports which backend the binary was compiled with. To force the issue
while testing, set `gpu = "on"`, which warns if no GPU backend is available.

## Building a CUDA binary yourself

Requires the CUDA Toolkit (13.x) plus the usual cmake + clang/MSVC toolchain:

```bash
cargo build --release \
  --manifest-path src/sempkg/Cargo.toml \
  --features reranker,embeddings,cuda
```

To control which GPU architectures are compiled in (the CI release pins a
Turing-through-Blackwell set), export `CMAKE_CUDA_ARCHITECTURES` before building
— the `llama-cpp-sys` build script forwards any `CMAKE_*` variable to CMake:

```bash
export CMAKE_CUDA_ARCHITECTURES="75-virtual;80-virtual;86-real;89-real;90-virtual"
```

To support pre-Turing GPUs (GTX 9xx/10xx, Titan V), build against the **CUDA
12.x** toolkit instead and add the older architectures, e.g. prepend
`50-virtual;61-virtual;70-virtual`. For non-NVIDIA GPUs, or when CUDA is
awkward, the vendor-neutral `vulkan` backend (`--features …,vulkan`) is often a
simpler alternative.
