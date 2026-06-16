# install.ps1 — Install sembundle and/or sempkg from GitHub Releases
#
# Usage:
#   Install both (default):
#     irm https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.ps1 | iex
#
#   Install a specific binary only:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.ps1))) -Only sembundle
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.ps1))) -Only sempkg
#
#   Install a specific version:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.ps1))) -Version v1.2.0

[CmdletBinding()]
param(
    [string] $Version = "latest",
    [ValidateSet("sembundle", "sempkg", "")]
    [string] $Only = "",
    [string] $InstallDir = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Repo = "willem445/codegraph-hub"
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

# ── Install ───────────────────────────────────────────────────────────────────
if ($Only -eq "" -or $Only -eq "sembundle") { Install-Binary "sembundle" }
if ($Only -eq "" -or $Only -eq "sempkg")    { Install-Binary "sempkg"    }

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
