# uninstall.ps1 — Remove sembundle and/or sempkg installed by install.ps1
#
# Usage:
#   Remove the binaries (default — leaves ~/.sempkg data untouched):
#     irm https://raw.githubusercontent.com/willem445/sempkg/main/uninstall.ps1 | iex
#
#   Remove the binaries AND the global sempkg data (~/.sempkg: bundles, models):
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/uninstall.ps1))) -Purge
#
#   Remove only one binary, or from a custom install directory:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/uninstall.ps1))) -Only sempkg
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/sempkg/main/uninstall.ps1))) -InstallDir C:\path
#
# The script is safe to re-run: anything already gone is reported and skipped.
# It never deletes per-project `<workspace>\.sempkg\` directories — those belong
# to your projects, not to the installation (they are listed as manual cleanup).

[CmdletBinding()]
param(
    [ValidateSet("sembundle", "sempkg", "")]
    [string] $Only = "",
    [string] $InstallDir = "",
    # Also delete the global data directory (~/.sempkg): global bundles, the
    # downloaded GGUF models (several GB), and the local-package registry.
    [switch] $Purge
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not $InstallDir) {
    $InstallDir = Join-Path $env:USERPROFILE ".local\bin"
}
# Exactly the directory sempkg itself uses (home_dir()/.sempkg). Deliberately not
# overridable: -Purge is a recursive delete, and an override the application does
# not honour could only point it at the wrong directory.
$DataDir = Join-Path $env:USERPROFILE ".sempkg"

Write-Host "Uninstalling from $InstallDir"

# ── Remove binaries (and, for the GPU install, the files unpacked beside them) ─
# install.ps1 expands the CUDA zip straight into $InstallDir, so sempkg.exe can
# be surrounded by CUDA runtime DLLs and a README-GPU.md. Those belong to the
# sempkg install and go with it; anything else in the directory does not.
$removed = 0

function Remove-InstalledFile {
    param([string] $Path, [switch] $Quiet)

    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Force
        Write-Host "  Removed: $Path"
        $script:removed++
    } elseif (-not $Quiet) {
        Write-Host "  Not installed: $Path"
    }
}

if ($Only -eq "" -or $Only -eq "sembundle") {
    Remove-InstalledFile (Join-Path $InstallDir "sembundle.exe")
}

if ($Only -eq "" -or $Only -eq "sempkg") {
    Remove-InstalledFile (Join-Path $InstallDir "sempkg.exe")

    foreach ($pattern in @("cudart64_*.dll", "cublas64_*.dll", "cublasLt64_*.dll")) {
        # -LiteralPath: the install dir is user-supplied and may contain `[`/`]`,
        # which -Path would interpret as a wildcard and match elsewhere. The
        # pattern itself belongs in -Filter, which is not a path.
        Get-ChildItem -LiteralPath $InstallDir -Filter $pattern -File -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-InstalledFile $_.FullName }
    }
    Remove-InstalledFile (Join-Path $InstallDir "README-GPU.md") -Quiet
}

# ── Global data (~/.sempkg) ───────────────────────────────────────────────────
function Get-DirSize {
    param([string] $Path)

    $bytes = (Get-ChildItem -LiteralPath $Path -Recurse -File -ErrorAction SilentlyContinue |
        Measure-Object -Property Length -Sum).Sum
    if (-not $bytes) { return "0 MB" }
    if ($bytes -ge 1GB) { return "{0:N1} GB" -f ($bytes / 1GB) }
    return "{0:N1} MB" -f ($bytes / 1MB)
}

if ($Purge) {
    if (Test-Path -LiteralPath $DataDir) {
        Write-Host ""
        Write-Host "Purging global data: $DataDir ($(Get-DirSize $DataDir))"
        Remove-Item -LiteralPath $DataDir -Recurse -Force
        Write-Host "  Removed: $DataDir"
    } else {
        Write-Host ""
        Write-Host "No global data at $DataDir — nothing to purge."
    }
} elseif (Test-Path -LiteralPath $DataDir) {
    Write-Host ""
    Write-Host "Kept: $DataDir ($(Get-DirSize $DataDir)) — global bundles, downloaded GGUF"
    Write-Host "      models, and the local-package registry. Re-run with -Purge to delete it, or:"
    Write-Host ""
    Write-Host "  Remove-Item -Recurse -Force `"$DataDir`""
}

# ── What we deliberately do not touch ─────────────────────────────────────────
Write-Host ""
Write-Host "Not removed (delete these yourself if you want them gone):"
Write-Host "  * <project>\.sempkg\, sempkg.toml, sempkg.lock - per-project workspace state"
Write-Host "  * <project>\.codegraph\ - CodeGraph indexes of your own repositories"
Write-Host "  * The CodeGraph CLI:  npm uninstall -g @colbymchenry/codegraph"
Write-Host "  * MCP server entries pointing at sempkg (e.g. .vscode\mcp.json)"

# ── User PATH ─────────────────────────────────────────────────────────────────
# install.ps1 appends $InstallDir to the user PATH, so uninstall should be able
# to take it back out — but that directory is a conventional place for other
# tools too (~/.local/bin). Only reclaim the PATH entry when the directory is
# empty (or gone) after the removals above, match the segment exactly, and say
# what is about to change before changing it.
#
# The value is read and written through the registry *unexpanded*, and its kind
# is preserved. [Environment]::GetEnvironmentVariable("PATH", "User") expands a
# REG_EXPAND_SZ value on read, so reading with it and writing the result back
# would bake every other tool's `%JAVA_HOME%\bin` / `%USERPROFILE%\go\bin`
# segment down to today's literal path and flip the value kind to REG_SZ —
# breaking those entries the moment the variable changes. Every segment this
# script does not remove must come back out exactly as it went in.
$dirIsEmpty = (-not (Test-Path -LiteralPath $InstallDir)) -or
              (-not (Get-ChildItem -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue))

$target = $InstallDir.TrimEnd('\')

# Compare on the *expanded* form (so an entry stored as `%USERPROFILE%\.local\bin`
# is still recognised as ours) but keep the raw form for writing back.
function Test-IsInstallDirSegment {
    param([string] $Segment)

    if (-not $Segment) { return $false }
    if ($Segment.TrimEnd('\') -eq $target) { return $true }
    return ([Environment]::ExpandEnvironmentVariables($Segment)).TrimEnd('\') -eq $target
}

$envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
try {
    $rawPath = $null
    $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    if ($envKey) {
        $rawPath = $envKey.GetValue(
            'Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -ne $rawPath) { $kind = $envKey.GetValueKind('Path') }
    }

    $segments = @()
    if ($rawPath) { $segments = @([string]$rawPath -split ';') }
    $onPath = @($segments | Where-Object { Test-IsInstallDirSegment $_ })

    if ($onPath.Count -gt 0) {
        Write-Host ""
        if ($dirIsEmpty) {
            # Drop *only* the matching segment(s): every other entry — including
            # empty ones from a stray `;;`, and any `%VAR%` still unexpanded — is
            # written back byte-for-byte as it was found, with the original kind.
            $kept = $segments | Where-Object { -not (Test-IsInstallDirSegment $_) }
            $newPath = ($kept -join ';')
            Write-Host "$InstallDir is empty and on your user PATH. Removing that entry:"
            foreach ($seg in $onPath) { Write-Host "  - $seg" }
            $envKey.SetValue('Path', $newPath, $kind)
            Write-Host "Removed. Restart your terminal for the change to take effect."
        } else {
            Write-Host "NOTE: $InstallDir is still on your user PATH and still contains other"
            Write-Host "      files, so it was left in place. Remove it by hand if you want it gone."
        }
    }
}
finally {
    if ($envKey) { $envKey.Dispose() }
}

Write-Host ""
Write-Host "Done."
