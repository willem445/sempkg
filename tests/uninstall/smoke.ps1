# tests/uninstall/smoke.ps1 — end-to-end check of uninstall.ps1.
#
# Everything happens inside a throwaway sandbox: a fake USERPROFILE (so the
# script's `~/.sempkg` is the sandbox's, never the real user's), a fake install
# dir, a "victim" directory, a canary directory, and a fake user workspace.
#
# The one piece of real machine state involved is the user PATH
# (HKCU\Environment), because removing its own segment is precisely the
# behaviour under test and there is no way to fake the registry key the script
# writes. It is handled with snapshot-and-restore: the raw (UNEXPANDED) value
# and its value kind are captured up front, a hostile probe value is written for
# the duration of the test, and the original value + kind are restored in a
# `finally` — which then asserts the restore succeeded. Nothing else on the
# machine is read or written.
#
# The pre-uninstall state is SEEDED DIRECTLY instead of by running install.ps1.
# That is deliberate: install.ps1 has a known pre-existing PATH-append bug
# (issue #107 — it expands REG_EXPAND_SZ entries on write-back), so a naive
# install -> uninstall round trip would fail on the installer's corruption
# rather than on anything uninstall does. Seeding the state an install *would*
# have produced isolates what uninstall actually owns. A full round trip is
# worth adding once #107 lands.
#
# Usage:
#   pwsh -File tests/uninstall/smoke.ps1 [-Script path\to\uninstall.ps1]
#
# -Script exists so the harness can be pointed at an older revision of the
# script to prove it catches the bugs it is meant to catch.

[CmdletBinding()]
param(
    [string] $Script = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $Script) { $Script = Join-Path $repoRoot "uninstall.ps1" }

Write-Host "Testing: $Script"

$script:failures = 0
function Assert-True {
    param([bool] $Condition, [string] $Message)

    if ($Condition) { Write-Host "  PASS: $Message" }
    else { Write-Host "  FAIL: $Message"; $script:failures++ }
}

# ── Sandbox ───────────────────────────────────────────────────────────────────
$sandboxRoot = Join-Path ([System.IO.Path]::GetTempPath()) "sempkg-uninstall-smoke-$PID"

function New-Sandbox {
    Remove-Item -LiteralPath $sandboxRoot -Recurse -Force -ErrorAction SilentlyContinue
    $script:HomeDir   = Join-Path $sandboxRoot "home"
    $script:BinDir    = Join-Path $script:HomeDir ".local\bin"
    $script:DataDir   = Join-Path $script:HomeDir ".sempkg"
    $script:Victim    = Join-Path $sandboxRoot "victim"
    $script:Canary    = Join-Path $sandboxRoot "canary"
    $script:Workspace = Join-Path $sandboxRoot "workspace"

    foreach ($d in @(
        $script:BinDir,
        (Join-Path $script:DataDir "bundles\demo\1.0.0"),
        (Join-Path $script:DataDir "models"),
        $script:Victim, $script:Canary,
        (Join-Path $script:Workspace ".sempkg\bundles")
    )) { New-Item -ItemType Directory -Force -Path $d | Out-Null }

    # What a completed GPU install leaves in the install dir: the two binaries,
    # the CUDA runtime DLLs install.ps1 unpacks from the zip, and README-GPU.md.
    foreach ($f in @(
        "sempkg.exe", "sembundle.exe",
        "cudart64_13.dll", "cublas64_13.dll", "cublasLt64_13.dll",
        "README-GPU.md"
    )) { Set-Content -LiteralPath (Join-Path $script:BinDir $f) -Value "x" }

    # An unrelated tool sharing the dir — must survive, and must keep the dir
    # "not empty" so the PATH entry is left alone.
    Set-Content -LiteralPath (Join-Path $script:BinDir "some-other-tool.exe") -Value "x"

    Set-Content -LiteralPath (Join-Path $script:DataDir "models\model.gguf") -Value "gguf"
    Set-Content -LiteralPath (Join-Path $script:DataDir "packages.json") -Value "{}"
    Set-Content -LiteralPath (Join-Path $script:Workspace ".sempkg\bundles\installed") -Value "bundle"
    Set-Content -LiteralPath (Join-Path $script:Victim "precious.txt") -Value "precious"
    Set-Content -LiteralPath (Join-Path $script:Canary "keep-me.txt") -Value "keep"
}

function Assert-BystandersUntouched {
    param([string] $Case)

    Assert-True (Test-Path -LiteralPath (Join-Path $script:Victim "precious.txt")) `
        "${Case}: victim dir untouched (SEMPKG_HOME ignored)"
    Assert-True (Test-Path -LiteralPath (Join-Path $script:Canary "keep-me.txt")) `
        "${Case}: canary dir untouched"
    Assert-True (Test-Path -LiteralPath (Join-Path $script:Workspace ".sempkg\bundles\installed")) `
        "${Case}: workspace .sempkg untouched"
}

# Run the uninstaller against the sandbox: USERPROFILE is redirected so the
# script's data dir is the sandbox's, and a hostile SEMPKG_HOME is exported to
# prove it is ignored.
function Invoke-Uninstall {
    # Hashtable splat, not an array: an array splat would pass "-Purge" as a
    # *positional* value (landing in -Only), not as the switch.
    param([hashtable] $UninstallArgs = @{})

    $savedProfile = $env:USERPROFILE
    $savedSempkgHome = $env:SEMPKG_HOME
    try {
        $env:USERPROFILE = $script:HomeDir
        $env:SEMPKG_HOME = $script:Victim
        & $Script -InstallDir $script:BinDir @UninstallArgs | Out-Null
        return $true
    } catch {
        Write-Host "  (uninstall threw: $($_.Exception.Message))"
        return $false
    } finally {
        $env:USERPROFILE = $savedProfile
        if ($null -eq $savedSempkgHome) { Remove-Item Env:\SEMPKG_HOME -ErrorAction SilentlyContinue }
        else { $env:SEMPKG_HOME = $savedSempkgHome }
    }
}

# ── PATH snapshot (the only real machine state touched) ───────────────────────
$envKey   = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
$origRaw  = $envKey.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
$origKind = if ($null -ne $origRaw) { $envKey.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }

function Get-RawUserPath {
    return [string]$envKey.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
}

try {
    # ── Case 1: dir keeps another tool -> binaries + CUDA files go, PATH stays ─
    Write-Host ""
    Write-Host "Case 1 — default run; install dir still holds an unrelated tool"
    New-Sandbox

    # Hostile PATH probe: REG_EXPAND_SZ, other tools' %VARs% left UNEXPANDED, a
    # stray empty segment, and our install dir in the middle.
    $probe = "%USERPROFILE%\fake\bin;%JAVA_HOME%\bin;;C:\other tools;$script:BinDir"
    $envKey.SetValue('Path', $probe, [Microsoft.Win32.RegistryValueKind]::ExpandString)

    Assert-True (Invoke-Uninstall) "Case 1: script completed"
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:BinDir "sempkg.exe")))    "Case 1: sempkg.exe removed"
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:BinDir "sembundle.exe"))) "Case 1: sembundle.exe removed"
    foreach ($dll in @("cudart64_13.dll", "cublas64_13.dll", "cublasLt64_13.dll")) {
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:BinDir $dll))) "Case 1: $dll removed"
    }
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:BinDir "README-GPU.md"))) "Case 1: README-GPU.md removed"
    Assert-True (Test-Path -LiteralPath (Join-Path $script:BinDir "some-other-tool.exe"))  "Case 1: unrelated tool KEPT"
    Assert-True (Test-Path -LiteralPath (Join-Path $script:DataDir "models\model.gguf"))   "Case 1: ~/.sempkg data KEPT (no -Purge)"
    Assert-True ((Get-RawUserPath) -eq $probe) "Case 1: PATH untouched while the dir is non-empty"
    Assert-BystandersUntouched "Case 1"

    # ── Case 2: dir now empty -> exactly our PATH segment is reclaimed ─────────
    Write-Host ""
    Write-Host "Case 2 — install dir now empty; PATH entry reclaimed"
    Remove-Item -LiteralPath (Join-Path $script:BinDir "some-other-tool.exe") -Force

    Assert-True (Invoke-Uninstall) "Case 2: script completed"

    $after    = Get-RawUserPath
    $expected = "%USERPROFILE%\fake\bin;%JAVA_HOME%\bin;;C:\other tools"
    $kind     = $envKey.GetValueKind('Path')

    Assert-True (-not $after.Contains($script:BinDir))       "Case 2: install dir removed from PATH"
    Assert-True ($after.Contains('%USERPROFILE%\fake\bin'))  "Case 2: %USERPROFILE% segment still UNEXPANDED"
    Assert-True ($after.Contains('%JAVA_HOME%\bin'))         "Case 2: %JAVA_HOME% segment still UNEXPANDED"
    Assert-True ($after.Contains(';;'))                      "Case 2: empty PATH segment preserved"
    Assert-True ($kind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString) "Case 2: value kind still REG_EXPAND_SZ"
    Assert-True ($after -eq $expected)                       "Case 2: every other PATH segment byte-identical"

    # ── Case 3: re-running changes nothing ────────────────────────────────────
    Write-Host ""
    Write-Host "Case 3 — idempotent re-run"
    Assert-True (Invoke-Uninstall) "Case 3: script completed on re-run"
    Assert-True ((Get-RawUserPath) -eq $expected) "Case 3: PATH unchanged by the re-run"
    Assert-True (Test-Path -LiteralPath (Join-Path $script:DataDir "models\model.gguf")) "Case 3: data still kept"

    # ── Case 4: -Purge removes exactly the sandbox ~/.sempkg ───────────────────
    Write-Host ""
    Write-Host "Case 4 — -Purge (with a hostile SEMPKG_HOME pointing at the victim dir)"
    New-Sandbox
    Assert-True (Invoke-Uninstall @{ Purge = $true }) "Case 4: script completed"
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:BinDir "sempkg.exe"))) "Case 4: sempkg.exe removed"
    Assert-True (-not (Test-Path -LiteralPath $script:DataDir))                         "Case 4: ~/.sempkg purged"
    Assert-BystandersUntouched "Case 4"

    # ── Case 5: -Only removes just the one binary ─────────────────────────────
    Write-Host ""
    Write-Host "Case 5 — -Only sempkg"
    New-Sandbox
    Assert-True (Invoke-Uninstall @{ Only = "sempkg" }) "Case 5: script completed"
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:BinDir "sempkg.exe"))) "Case 5: sempkg.exe removed"
    Assert-True (Test-Path -LiteralPath (Join-Path $script:BinDir "sembundle.exe"))     "Case 5: sembundle.exe KEPT"
    Assert-True (Test-Path -LiteralPath (Join-Path $script:DataDir "models\model.gguf")) "Case 5: ~/.sempkg data KEPT"
    Assert-BystandersUntouched "Case 5"
}
finally {
    # Restore the user PATH exactly as found — raw value and value kind.
    if ($null -eq $origRaw) { $envKey.DeleteValue('Path', $false) }
    else { $envKey.SetValue('Path', $origRaw, $origKind) }

    $restoredRaw  = $envKey.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $restoredKind = if ($null -ne $origRaw) { $envKey.GetValueKind('Path') } else { $origKind }
    Write-Host ""
    Assert-True ($restoredRaw -eq $origRaw)   "Teardown: user PATH restored (raw value identical)"
    Assert-True ($restoredKind -eq $origKind) "Teardown: user PATH restored (value kind identical)"
    $envKey.Dispose()

    Remove-Item -LiteralPath $sandboxRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
if ($script:failures -eq 0) {
    Write-Host "uninstall.ps1: all checks passed"
    exit 0
}
Write-Host "uninstall.ps1: $($script:failures) check(s) FAILED"
exit 1
