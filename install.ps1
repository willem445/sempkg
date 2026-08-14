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
# Read and write the user PATH through the registry *unexpanded*, preserving its
# value kind. [Environment]::GetEnvironmentVariable("PATH", "User") expands a
# REG_EXPAND_SZ value on read, so reading with it and writing the result back
# would bake every other tool's `%JAVA_HOME%\bin` / `%USERPROFILE%\go\bin`
# segment down to today's literal path and flip the value kind to REG_SZ —
# breaking those entries the moment the variable changes. Every existing
# segment must come back out exactly as it went in (see uninstall.ps1, which
# fixed the same bug on removal — issue #107).
$target = $InstallDir.TrimEnd('\')

# Compare on the *expanded* form (so an entry already stored as
# `%USERPROFILE%\.local\bin` is recognised as present) but keep the raw form
# for writing back. A plain substring/`-like` match on the raw value would miss
# that case and append a duplicate entry on every run.
function Test-IsInstallDirSegment {
    param([string] $Segment)

    if (-not $Segment) { return $false }
    if ($Segment.TrimEnd('\') -eq $target) { return $true }
    return ([Environment]::ExpandEnvironmentVariables($Segment)).TrimEnd('\') -eq $target
}

$envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
try {
    $rawPath = $null
    $kind = [Microsoft.Win32.RegistryValueKind]::String
    if ($envKey) {
        $rawPath = $envKey.GetValue(
            'Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -ne $rawPath) { $kind = $envKey.GetValueKind('Path') }
    }

    $segments = @()
    if ($rawPath) { $segments = @([string]$rawPath -split ';') }
    $alreadyPresent = @($segments | Where-Object { Test-IsInstallDirSegment $_ }).Count -gt 0

    if ($alreadyPresent) {
        Write-Host "Done."
    } elseif (-not $envKey) {
        # HKCU\Environment should always be openable; this is a last-resort
        # fallback so install still completes rather than crashing. It can
        # still expand %VAR% segments already in the user PATH — said so
        # explicitly rather than silently.
        Write-Host ""
        Write-Host "WARNING: could not open HKCU\Environment for writing; falling back to"
        Write-Host "         [Environment]::SetEnvironmentVariable, which may expand any"
        Write-Host "         %VAR% segments already in your user PATH."
        $legacyPath = [Environment]::GetEnvironmentVariable("PATH", "User")
        $newPath = if ($legacyPath) { $legacyPath.TrimEnd(";") + ";" + $InstallDir } else { $InstallDir }
        [Environment]::SetEnvironmentVariable("PATH", $newPath, "User")
        $env:PATH = $env:PATH.TrimEnd(";") + ";" + $InstallDir
        Write-Host "Done. Restart your terminal for the change to take effect."
    } else {
        Write-Host ""
        Write-Host "NOTE: $InstallDir is not on your PATH."
        Write-Host "Adding it to your user PATH permanently..."
        # Preserve every existing segment byte-for-byte — including stray empty
        # ones from a `;;` — and append the install dir.
        $newPath = (@($segments) + $InstallDir) -join ';'
        $envKey.SetValue('Path', $newPath, $kind)
        $env:PATH = $env:PATH.TrimEnd(";") + ";" + $InstallDir
        Write-Host "Done. Restart your terminal for the change to take effect."
    }
}
finally {
    if ($envKey) { $envKey.Dispose() }
}
