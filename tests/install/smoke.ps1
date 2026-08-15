# tests/install/smoke.ps1 — end-to-end check of install.ps1's user-PATH update.
#
# install.ps1 also downloads real binaries from GitHub Releases, which this
# harness must not do. Network cmdlets are shadowed with local mock functions
# before invoking the real script — `Invoke-WebRequest` just drops a marker
# file at the requested destination — and an explicit -Version plus -Gpu off
# skip the "latest" release lookup and the CUDA/GPU download path entirely, so
# nothing here ever touches the network. PowerShell command resolution favors
# a function over a cmdlet of the same name for the whole call chain below the
# point it is defined, which is what makes the shadowing work through `&`.
#
# Everything else happens inside a throwaway sandbox: a fake USERPROFILE (so
# the script's default install dir and any %USERPROFILE%-relative PATH
# segments resolve inside the sandbox, never on the real machine) and a fake
# install dir.
#
# The one piece of real machine state involved is the user PATH
# (HKCU\Environment), because appending to it is precisely the behaviour under
# test and there is no way to fake the registry key the script writes. It is
# handled with snapshot-and-restore: the raw (UNEXPANDED) value and its value
# kind are captured up front, a hostile probe value (or no value at all) is
# written for the duration of each case, and the original value + kind are
# restored in a `finally` — which then asserts the restore succeeded. Nothing
# else on the machine is read or written.
#
# Usage:
#   pwsh -File tests/install/smoke.ps1 [-Script path\to\install.ps1]
#
# -Script exists so the harness can be pointed at an older revision of the
# script to prove it catches the bug it is meant to catch (issue #107).

[CmdletBinding()]
param(
    [string] $Script = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $Script) { $Script = Join-Path $repoRoot "install.ps1" }

Write-Host "Testing: $Script"

$script:failures = 0
function Assert-True {
    param([bool] $Condition, [string] $Message)

    if ($Condition) { Write-Host "  PASS: $Message" }
    else { Write-Host "  FAIL: $Message"; $script:failures++ }
}

# ── Sandbox ───────────────────────────────────────────────────────────────────
$sandboxRoot = Join-Path ([System.IO.Path]::GetTempPath()) "sempkg-install-smoke-$PID"

function New-Sandbox {
    Remove-Item -LiteralPath $sandboxRoot -Recurse -Force -ErrorAction SilentlyContinue
    $script:HomeDir = Join-Path $sandboxRoot "home"
    $script:BinDir  = Join-Path $script:HomeDir ".local\bin"
    New-Item -ItemType Directory -Force -Path $script:BinDir | Out-Null
}

# Run the installer against the sandbox, with the network cmdlets it calls
# shadowed by no-op mocks so nothing here ever reaches GitHub. USERPROFILE is
# redirected so the script's default install dir, and any %USERPROFILE%
# expansion in a PATH segment, resolve inside the sandbox.
function Invoke-Install {
    param([hashtable] $InstallArgs = @{})

    # Shadows the cmdlet of the same name for every command PowerShell resolves
    # from this point in the call chain onward, including inside `& $Script`.
    function Invoke-WebRequest {
        param([string] $Uri, [string] $OutFile, [switch] $UseBasicParsing)
        Set-Content -LiteralPath $OutFile -Value "fake-binary"
    }

    $savedProfile = $env:USERPROFILE
    try {
        $env:USERPROFILE = $script:HomeDir
        & $Script -Version "v0.0.0-test" -InstallDir $script:BinDir -Only sempkg -Gpu off @InstallArgs | Out-Null
        return $true
    } catch {
        Write-Host "  (install threw: $($_.Exception.Message))"
        return $false
    } finally {
        $env:USERPROFILE = $savedProfile
    }
}

# ── PATH snapshot (the only real machine state touched) ───────────────────────
$envKey   = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
$origRaw  = $envKey.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
$origKind = if ($null -ne $origRaw) { $envKey.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }

function Get-RawUserPath {
    return [string]$envKey.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
}

function Get-RawUserPathKind {
    return $envKey.GetValueKind('Path')
}

function Set-RawUserPath {
    param([string] $Value, $Kind)
    if ($null -eq $Value) { $envKey.DeleteValue('Path', $false) }
    else { $envKey.SetValue('Path', $Value, $Kind) }
}

try {
    # ── Case 1: no existing user PATH value at all ─────────────────────────────
    Write-Host ""
    Write-Host "Case 1 — no existing user PATH value (must not crash on null)"
    New-Sandbox
    $envKey.DeleteValue('Path', $false) 2>$null

    Assert-True (Invoke-Install) "Case 1: install completed without throwing"
    $after = Get-RawUserPath
    Assert-True ($after -eq $script:BinDir) "Case 1: PATH created containing exactly the install dir"
    Assert-True ((Get-RawUserPathKind) -eq [Microsoft.Win32.RegistryValueKind]::String) "Case 1: new value created as REG_SZ"

    # ── Case 2: REG_EXPAND_SZ PATH with unexpanded %VAR% segments, dir absent ──
    Write-Host ""
    Write-Host "Case 2 — REG_EXPAND_SZ PATH with %VAR% segments; install dir not yet present"
    New-Sandbox

    # Hostile PATH probe: REG_EXPAND_SZ, other tools' %VARs% left UNEXPANDED, a
    # stray empty segment.
    $probe = "%USERPROFILE%\fake\bin;%JAVA_HOME%\bin;;C:\other tools"
    Set-RawUserPath $probe ([Microsoft.Win32.RegistryValueKind]::ExpandString)

    Assert-True (Invoke-Install) "Case 2: install completed"

    $after = Get-RawUserPath
    $kind  = $envKey.GetValueKind('Path')
    $expected = "$probe;$script:BinDir"

    Assert-True ($after -eq $expected) "Case 2: every existing segment preserved byte-for-byte, install dir appended"
    Assert-True ($after.Contains('%USERPROFILE%\fake\bin')) "Case 2: %USERPROFILE% segment still UNEXPANDED"
    Assert-True ($after.Contains('%JAVA_HOME%\bin'))        "Case 2: %JAVA_HOME% segment still UNEXPANDED"
    Assert-True ($after.Contains(';;'))                     "Case 2: stray empty segment preserved"
    Assert-True ($kind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString) "Case 2: value kind still REG_EXPAND_SZ"

    # ── Case 3: idempotent re-run — dir now literally present ──────────────────
    Write-Host ""
    Write-Host "Case 3 — re-run with the install dir already a literal PATH segment"
    Assert-True (Invoke-Install) "Case 3: install completed on re-run"
    Assert-True ((Get-RawUserPath) -eq $expected) "Case 3: PATH unchanged — no duplicate entry appended"

    # ── Case 4: install dir present only in UNEXPANDED %VAR% form (the trap) ───
    Write-Host ""
    Write-Host "Case 4 — install dir already on PATH, but only in unexpanded %USERPROFILE% form"
    New-Sandbox
    # %USERPROFILE% expands (via the redirected USERPROFILE) to $script:HomeDir,
    # so "%USERPROFILE%\.local\bin" expands to exactly $script:BinDir.
    $probe4 = "%USERPROFILE%\.local\bin;C:\other tools"
    Set-RawUserPath $probe4 ([Microsoft.Win32.RegistryValueKind]::ExpandString)

    Assert-True (Invoke-Install) "Case 4: install completed"
    $after4 = Get-RawUserPath
    Assert-True ($after4 -eq $probe4) "Case 4: PATH unchanged — expanded-form match prevented a duplicate"
    Assert-True (-not ($after4 -match [regex]::Escape($script:BinDir))) `
        "Case 4: no literal duplicate of the install dir was appended"
}
finally {
    # Restore the user PATH exactly as found — raw value and value kind.
    Set-RawUserPath $origRaw $origKind

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
    Write-Host "install.ps1: all checks passed"
    exit 0
}
Write-Host "install.ps1: $($script:failures) check(s) FAILED"
exit 1
