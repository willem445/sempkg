# install.ps1 — Install sembundle and/or sempkg from GitHub Releases
#
# Usage:
#   Install both (default):
#     irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1 | iex
#
#   Install a specific binary only:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1))) -Only sembundle
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1))) -Only sempkg
#
#   Install a specific version:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1))) -Version v1.2.0
#
#   Force the CPU build (or force the GPU build) for sempkg:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1))) -Gpu off
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1))) -Gpu on

[CmdletBinding()]
param(
    [string] $Version = "latest",
    [ValidateSet("sembundle", "sempkg", "")]
    [string] $Only = "",
    [string] $InstallDir = "",
    # GPU build selection for sempkg:
    #   auto (default) — install the CUDA/GPU build when a supported NVIDIA GPU
    #                    and driver are detected, otherwise the CPU build
    #   on             — force the CUDA/GPU build
    #   off            — force the CPU build
    [ValidateSet("auto", "on", "off")]
    [string] $Gpu = "auto"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Repo = "willem445/sempkg"
$Target = "x86_64-pc-windows-msvc"

# ── Default install directory ─────────────────────────────────────────────────
if (-not $InstallDir) {
    $InstallDir = Join-Path $env:USERPROFILE ".local\bin"
}

# ── Resolve latest version tag ────────────────────────────────────────────────
if ($Version -eq "latest") {
    $apiUrl = "https://api.github.com/repos/$Repo/releases/latest"
    $release = Invoke-RestMethod -Uri $apiUrl -Headers @{ "User-Agent" = "install.ps1" }
    $Version = $release.tag_name
}

Write-Host "Installing version $Version for $Target"

# ── Create install directory ──────────────────────────────────────────────────
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# ── Download helper ───────────────────────────────────────────────────────────
function Install-Binary {
    param([string] $Binary)

    $fileName = "$Binary-$Target.exe"
    $url  = "https://github.com/$Repo/releases/download/$Version/$fileName"
    $dest = Join-Path $InstallDir "$Binary.exe"

    Write-Host "  Downloading $Binary from $url ..."
    Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
    Write-Host "  Installed: $dest"
}

# ── GPU detection ─────────────────────────────────────────────────────────────
# True when an NVIDIA GPU with compute capability >= 7.5 (Turing) and a driver
# new enough for the CUDA 13 build (>= 580) is present. nvidia-smi only exists
# when a driver is installed, so its absence is a definitive "no GPU".
function Test-CudaSupported {
    if (-not (Get-Command nvidia-smi -ErrorAction SilentlyContinue)) { return $false }
    try {
        $caps = & nvidia-smi --query-gpu=compute_cap   --format=csv,noheader 2>$null
        $drv  = & nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>$null
        $inv = [System.Globalization.CultureInfo]::InvariantCulture
        $capValues = @($caps | Where-Object { $_ -and $_.Trim() } | ForEach-Object { [double]::Parse($_.Trim(), $inv) })
        if ($capValues.Count -eq 0) { return $false }
        $maxCap   = ($capValues | Measure-Object -Maximum).Maximum
        $drvMajor = if ($drv) { [int]((@($drv)[0]).Trim().Split('.')[0]) } else { 0 }
    } catch { return $false }

    if ($maxCap -ge 7.5 -and $drvMajor -ge 580) { return $true }
    if ($maxCap -ge 7.5) {
        Write-Host "  NVIDIA GPU (compute $maxCap) found, but driver $drv is older than 580 (required for the CUDA 13 build) — installing CPU build."
    } else {
        Write-Host "  NVIDIA GPU compute capability $maxCap is below 7.5 (Turing) — installing CPU build."
    }
    return $false
}

# Install sempkg, preferring the CUDA/GPU build when appropriate. The GPU build
# ships as a zip containing sempkg.exe plus the CUDA runtime DLLs, all extracted
# side-by-side into $InstallDir so Windows loads the DLLs from the exe's folder.
function Install-Sempkg {
    $useGpu = switch ($Gpu) {
        "on"    { $true }
        "off"   { $false }
        default { Test-CudaSupported }   # auto
    }

    if (-not $useGpu) { Install-Binary "sempkg"; return }

    $fileName = "sempkg-$Target-cuda.zip"
    $url      = "https://github.com/$Repo/releases/download/$Version/$fileName"
    $tmpZip   = Join-Path ([System.IO.Path]::GetTempPath()) $fileName

    Write-Host "  Downloading sempkg (CUDA/GPU build) from $url ..."
    try {
        Invoke-WebRequest -Uri $url -OutFile $tmpZip -UseBasicParsing
    } catch {
        Write-Host "  CUDA build not available for $Version — falling back to CPU build."
        Install-Binary "sempkg"
        return
    }

    Expand-Archive -Path $tmpZip -DestinationPath $InstallDir -Force
    Remove-Item $tmpZip -Force -ErrorAction SilentlyContinue
    Write-Host "  Installed GPU build + bundled CUDA runtime DLLs to $InstallDir"
    Write-Host "  (requires an NVIDIA driver >= 580; no CUDA Toolkit needed)"
}

# ── Install ───────────────────────────────────────────────────────────────────
if ($Only -eq "" -or $Only -eq "sembundle") { Install-Binary "sembundle" }
if ($Only -eq "" -or $Only -eq "sempkg")    { Install-Sempkg }

# ── PATH check ────────────────────────────────────────────────────────────────
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($userPath -notlike "*$InstallDir*") {
    Write-Host ""
    Write-Host "NOTE: $InstallDir is not on your PATH."
    Write-Host "Adding it to your user PATH permanently..."
    $newPath = ($userPath.TrimEnd(";") + ";" + $InstallDir).TrimStart(";")
    [Environment]::SetEnvironmentVariable("PATH", $newPath, "User")
    $env:PATH = $env:PATH.TrimEnd(";") + ";" + $InstallDir
    Write-Host "Done. Restart your terminal for the change to take effect."
} else {
    Write-Host "Done."
}
