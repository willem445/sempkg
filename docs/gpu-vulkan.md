# GPU acceleration (Vulkan)

sempkg runs its embedding, query-expansion, and reranker models in-process
through llama.cpp. GPU offload is a **build-time** capability: the standard
release binaries are CPU-only, and a GPU build offloads these models to the GPU.

Vulkan is the **vendor-neutral** GPU backend. Use it when the
[CUDA build](gpu-cuda.md) does not fit:

- **AMD and Intel GPUs** — CUDA is NVIDIA-only.
- **Pre-Turing NVIDIA cards** (GTX 9xx / 10xx, Titan V). The CUDA artifacts
  target CUDA 13, which dropped Maxwell, Pascal, and Volta.
- Machines where you would rather not install an NVIDIA driver ≥ 580.

The release workflow publishes Vulkan artifacts alongside the CPU and CUDA ones:

| Artifact | Contents |
| --- | --- |
| `sempkg-x86_64-pc-windows-msvc-vulkan.exe` | single binary |
| `sempkg-x86_64-unknown-linux-gnu-vulkan` | single binary |

## Installing

The install scripts do **not** pick the Vulkan build automatically — they choose
between the CPU and CUDA builds only. Download the artifact from the
[release page](https://github.com/willem445/sempkg/releases), rename it to
`sempkg` (`sempkg.exe` on Windows), and place it on your `PATH` — for example in
`~/.local/bin`, where the install scripts put the other binaries:

```sh
# Linux
curl -fsSL -o ~/.local/bin/sempkg \
  https://github.com/willem445/sempkg/releases/latest/download/sempkg-x86_64-unknown-linux-gnu-vulkan
chmod +x ~/.local/bin/sempkg
```

```powershell
# Windows
irm https://github.com/willem445/sempkg/releases/latest/download/sempkg-x86_64-pc-windows-msvc-vulkan.exe `
  -OutFile "$env:USERPROFILE\.local\bin\sempkg.exe"
```

## What you need to run it

Unlike the CUDA build there is nothing to bundle — the Vulkan **loader** is part
of the system, not of sempkg:

- **Windows:** `vulkan-1.dll` ships with the GPU driver (NVIDIA, AMD, and Intel
  all install it). Keep your GPU driver current; nothing else is required.
- **Linux:** install the loader and your GPU's Vulkan driver from the distro,
  e.g. `sudo apt install libvulkan1 mesa-vulkan-drivers` (Mesa covers AMD and
  Intel; NVIDIA's proprietary driver provides its own ICD).

No Vulkan SDK is needed at runtime — the SDK is only used to *build* the binary.

## Verifying GPU offload

`gpu = "auto"` (the default in the `[embedding]`, `[query_expansion]`, and
`[reranker]` config sections) offloads to the GPU when the binary has a working
GPU backend and silently falls back to the CPU otherwise. Check what you are
running with:

```sh
sempkg status
```

`features` should list `vulkan` and `gpu build` should read
`compiled with vulkan`. If it says `CPU-only`, you are running a CPU artifact.
To make a failure loud while testing, set `gpu = "on"` — it warns when no GPU
backend is compiled in.

## Building a Vulkan binary yourself

Requires the [Vulkan SDK](https://vulkan.lunarg.com/sdk/home) (it provides the
headers, the loader import library, and the `glslc` shader compiler that ggml
uses to compile its Vulkan shaders at build time) plus the usual cmake +
clang/MSVC toolchain:

```bash
cargo build --release \
  --manifest-path src/sempkg/Cargo.toml \
  --features reranker,embeddings,vulkan
```

On Windows the build reads `VULKAN_SDK` (set by the LunarG installer) to find
`Lib\vulkan-1.lib`. On Linux the distro packages
(`sudo apt install vulkan-sdk` from LunarG's apt repository, or
`libvulkan-dev` + `glslc`) are enough.

Note that a GPU is not needed to *build* — CI builds these artifacts on GPU-less
runners. A GPU and its driver are only needed to run with offload enabled.
