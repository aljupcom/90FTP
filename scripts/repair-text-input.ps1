<#
.SYNOPSIS
    Diagnoses (and optionally repairs) the Windows text-input service crash that
    surfaces as "ctfmon.exe - Fail Fast Exception".

.DESCRIPTION
    ctfmon.exe is the Windows Text Services process. It can fail fast inside a
    stale system component, or after a third-party antivirus hook is injected
    into it. Both cases are machine-level defects: the application that happens
    to be in the foreground only triggers the code path, it does not cause it.

    Default mode is read-only. It reports:
      * the OS servicing revision (UBR),
      * every text-services component whose build revision lags behind it,
      * third-party DLLs found inside the newest ctfmon crash dump,
      * how many ctfmon crash dumps exist and when the last one was written.

    With -Repair it runs DISM and SFC (requires an elevated shell) to replace
    the stale component with the one that matches the installed OS.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\scripts\repair-text-input.ps1

.EXAMPLE
    # elevated shell
    powershell -ExecutionPolicy Bypass -File .\scripts\repair-text-input.ps1 -Repair
#>
param(
    [switch]$Repair,
    [switch]$SkipDism
)

$ErrorActionPreference = "Continue"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

# Components that make up the text-input stack. All of them are serviced
# together by cumulative updates, so their build revision must match the OS.
$textServices = @(
    "ctfmon.exe",
    "msctf.dll",
    "imm32.dll",
    "CoreUIComponents.dll",
    "TextInputFramework.dll",
    "InputService.dll",
    "inputhost.dll",
    "MsCtfMonitor.DLL"
)

function Get-SystemBuild {
    $cv = Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion"
    [pscustomobject]@{
        Product = $cv.ProductName
        Display = $cv.DisplayVersion
        Build   = [int]$cv.CurrentBuild
        Ubr     = [int]$cv.UBR
    }
}

# FileVersion strings look like "10.0.26100.9549 (WinBuild.160101.0800)".
# Only the fourth field is the servicing revision that tracks the OS UBR.
function Get-ComponentRevision {
    param([string]$Version)
    if (-not $Version) { return $null }
    $clean = ($Version -split " ")[0]
    $parts = $clean.Split(".")
    if ($parts.Count -lt 4) { return $null }
    $rev = 0
    if (-not [int]::TryParse($parts[3], [ref]$rev)) { return $null }
    return $rev
}

function Get-MinidumpModules {
    <#
        Reads the module list out of a minidump without any external tooling.
        Enough to answer "which non-Microsoft DLL was loaded in the process
        that crashed", which is the whole point of the exercise.
    #>
    param([string]$Path)

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 32) { return @() }
    if ([System.Text.Encoding]::ASCII.GetString($bytes, 0, 4) -ne "MDMP") { return @() }

    $streamCount = [BitConverter]::ToUInt32($bytes, 8)
    $dirRva = [BitConverter]::ToUInt32($bytes, 12)

    $moduleRva = $null
    for ($i = 0; $i -lt $streamCount; $i++) {
        $entry = $dirRva + ($i * 12)
        if ($entry + 12 -gt $bytes.Length) { break }
        $type = [BitConverter]::ToUInt32($bytes, $entry)
        if ($type -eq 4) {
            $moduleRva = [BitConverter]::ToUInt32($bytes, $entry + 8)
            break
        }
    }
    if ($null -eq $moduleRva) { return @() }

    $count = [BitConverter]::ToUInt32($bytes, $moduleRva)
    if ($count -gt 4096) { return @() }

    $list = New-Object System.Collections.Generic.List[object]
    $offset = $moduleRva + 4
    for ($m = 0; $m -lt $count; $m++) {
        $entry = $offset + ($m * 108)
        if ($entry + 24 -gt $bytes.Length) { break }
        $base = [BitConverter]::ToUInt64($bytes, $entry)
        $size = [BitConverter]::ToUInt32($bytes, $entry + 8)
        $nameRva = [BitConverter]::ToUInt32($bytes, $entry + 20)
        if ($nameRva -le 0 -or ($nameRva + 4) -ge $bytes.Length) { continue }
        $byteLen = [BitConverter]::ToUInt32($bytes, $nameRva)
        if ($byteLen -le 0 -or ($nameRva + 4 + $byteLen) -gt $bytes.Length) { continue }
        $name = [System.Text.Encoding]::Unicode.GetString($bytes, $nameRva + 4, $byteLen)
        $list.Add([pscustomobject]@{ Base = $base; Size = $size; Name = $name })
    }
    return $list
}

function Get-MinidumpFault {
    <#
        Returns the exception address and the module that owns it. This is what
        makes the diagnosis factual instead of a guess based on load order.
    #>
    param([string]$Path)

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 32) { return $null }
    if ([System.Text.Encoding]::ASCII.GetString($bytes, 0, 4) -ne "MDMP") { return $null }

    $streamCount = [BitConverter]::ToUInt32($bytes, 8)
    $dirRva = [BitConverter]::ToUInt32($bytes, 12)

    $exceptionRva = $null
    for ($i = 0; $i -lt $streamCount; $i++) {
        $entry = $dirRva + ($i * 12)
        if ($entry + 12 -gt $bytes.Length) { break }
        if ([BitConverter]::ToUInt32($bytes, $entry) -eq 6) {
            $exceptionRva = [BitConverter]::ToUInt32($bytes, $entry + 8)
            break
        }
    }
    if ($null -eq $exceptionRva) { return $null }

    # MINIDUMP_EXCEPTION_STREAM: ThreadId(4) + alignment(4) + ExceptionRecord.
    # ExceptionRecord: Code(4) Flags(4) Record(8) Address(8).
    $code = [BitConverter]::ToUInt32($bytes, $exceptionRva + 8)
    $address = [BitConverter]::ToUInt64($bytes, $exceptionRva + 24)
    return [pscustomobject]@{ Code = $code; Address = $address }
}

function Test-ThirdPartyModules {
    param($Modules)

    # Anything outside the Windows directory is, by definition, not shipped by
    # Microsoft. That is where AV / DLP / "protection" hooks show up.
    $foreign = @()
    foreach ($mod in $Modules) {
        if (-not $mod -or -not $mod.Name) { continue }
        if ($mod.Name -match '^[A-Za-z]:\\Windows\\') { continue }
        $foreign += $mod.Name
    }
    return $foreign
}

Write-Host ""
Write-Host "90FTP - Windows text input (ctfmon.exe) diagnostic" -ForegroundColor Cyan
Write-Host ("=" * 62) -ForegroundColor DarkGray

$build = Get-SystemBuild
Write-Host ("OS          : {0} {1}  build {2}.{3}" -f $build.Product, $build.Display, $build.Build, $build.Ubr)

# ---------------------------------------------------------------- components
Write-Host ""
Write-Host "Text-services components" -ForegroundColor Cyan

$stale = @()
foreach ($name in $textServices) {
    $path = Join-Path "C:\Windows\System32" $name
    if (-not (Test-Path $path)) {
        Write-Host ("  {0,-24} not present" -f $name) -ForegroundColor DarkGray
        continue
    }
    $info = Get-Item $path
    $revision = Get-ComponentRevision $info.VersionInfo.FileVersion
    $version = $info.VersionInfo.FileVersion

    if ($null -eq $revision -or $revision -le 1) {
        # "26100.1" is the RTM stamp: these binaries are not revision-stamped
        # and must not be flagged as stale.
        Write-Host ("  {0,-24} {1}" -f $name, $version)
        continue
    }
    if ($revision -lt $build.Ubr) {
        Write-Host ("  {0,-24} {1}  <-- BEHIND by {2}" -f $name, $version, ($build.Ubr - $revision)) -ForegroundColor Red
        $stale += [pscustomobject]@{ Name = $name; Version = $version; Revision = $revision }
    }
    else {
        Write-Host ("  {0,-24} {1}" -f $name, $version)
    }
}

# -------------------------------------------------------------------- dumps
Write-Host ""
Write-Host "ctfmon crash dumps" -ForegroundColor Cyan

$dumpDir = Join-Path $env:LOCALAPPDATA "CrashDumps"
$dumps = @()
$modules = @()
$fault = $null
$faultModule = $null
if (Test-Path $dumpDir) {
    $dumps = @(Get-ChildItem $dumpDir -Filter "ctfmon*.dmp" | Sort-Object LastWriteTime)
}
if ($dumps.Count -eq 0) {
    Write-Host "  none found in the local crash dump folder" -ForegroundColor DarkGray
}
else {
    $newest = $dumps[-1]
    Write-Host ("  {0} dump(s), newest: {1}" -f $dumps.Count, $newest.LastWriteTime)
    $modules = @(Get-MinidumpModules $newest.FullName)
    $fault = Get-MinidumpFault $newest.FullName

    if ($fault) {
        Write-Host ("  exception {0} at 0x{1:X}" -f ("0x{0:X8}" -f $fault.Code), $fault.Address)
        foreach ($mod in $modules) {
            if ($fault.Address -ge $mod.Base -and $fault.Address -lt ($mod.Base + $mod.Size)) {
                $faultModule = $mod
                break
            }
        }
        if ($faultModule) {
            $leaf = Split-Path $faultModule.Name -Leaf
            Write-Host ("  faulting module: {0} + 0x{1:X}" -f $leaf, ($fault.Address - $faultModule.Base)) -ForegroundColor Red
        }
    }

    $foreign = Test-ThirdPartyModules $modules
    if ($foreign.Count -eq 0) {
        Write-Host "  no third-party module was loaded in the crashing process" -ForegroundColor Green
    }
    else {
        Write-Host "  third-party modules loaded in the crashing process:" -ForegroundColor Yellow
        foreach ($mod in $foreign) { Write-Host ("    - {0}" -f $mod) -ForegroundColor Yellow }
    }
}

# ------------------------------------------------------------------ verdict
Write-Host ""
Write-Host "Verdict" -ForegroundColor Cyan

# Prefer the module the crash dump actually points at; fall back to the most
# outdated one when no dump is available.
$culprit = $null
if ($faultModule) {
    $leaf = Split-Path $faultModule.Name -Leaf
    $culprit = $stale | Where-Object { $_.Name -eq $leaf } | Select-Object -First 1
    if ($culprit) {
        Write-Host ("  {0} fails fast and its revision ({1}) is behind the OS ({2})." -f $culprit.Name, $culprit.Revision, $build.Ubr) -ForegroundColor Yellow
        Write-Host "  This is a servicing defect: the component was left behind by a"
        Write-Host "  partial cumulative update. Replacing it removes the crash."
    }
    else {
        Write-Host ("  The crash points at {0}, which matches the installed OS revision." -f $leaf) -ForegroundColor Yellow
        Write-Host "  In that case the trigger is the injected third-party module below."
    }
}
elseif ($stale.Count -gt 0) {
    $culprit = $stale[0]
    Write-Host ("  {0} is {1} revision(s) behind the installed OS." -f $culprit.Name, ($build.Ubr - $culprit.Revision)) -ForegroundColor Yellow
}

if ($culprit) {
    Write-Host "  Repair: run this script again from an elevated shell with -Repair" -ForegroundColor Yellow
}
elseif ($stale.Count -eq 0) {
    Write-Host "  No stale text-services component detected." -ForegroundColor Green
}

$foreign = Test-ThirdPartyModules $modules
if ($foreign.Count -gt 0) {
    Write-Host ""
    Write-Host "  An antivirus / protection hook is injected into ctfmon.exe." -ForegroundColor Yellow
    Write-Host "  Even with a healthy component, such a hook can still crash it."
    Write-Host "  Add ctfmon.exe to that product's exclusion list, or update it."
}

if (-not $Repair) {
    Write-Host ""
    Write-Host "Read-only run finished. Nothing was changed." -ForegroundColor DarkGray
    return
}

# ------------------------------------------------------------------- repair
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Repair needs an elevated shell. Right-click PowerShell, choose 'Run as administrator', and run the same command again."
}

Write-Host ""
Write-Host "Repairing the component store" -ForegroundColor Cyan

if (-not $SkipDism) {
    Write-Host ">>> DISM /Online /Cleanup-Image /RestoreHealth"
    & dism.exe /Online /Cleanup-Image /RestoreHealth
}

Write-Host ">>> sfc /scannow"
& sfc.exe /scannow

Write-Host ""
Write-Host "Post-repair component check" -ForegroundColor Cyan
$after = Get-SystemBuild
$stillStale = @()
foreach ($name in $textServices) {
    $path = Join-Path "C:\Windows\System32" $name
    if (-not (Test-Path $path)) { continue }
    $revision = Get-ComponentRevision (Get-Item $path).VersionInfo.FileVersion
    if ($null -ne $revision -and $revision -gt 1 -and $revision -lt $after.Ubr) {
        Write-Host ("  {0,-24} still behind" -f $name) -ForegroundColor Red
        $stillStale += $name
    }
}

if ($stillStale.Count -eq 0) {
    Write-Host "  every text-services component now matches build $($after.Build).$($after.Ubr)" -ForegroundColor Green
    Write-Host "  Restart Windows to let the new binaries take effect." -ForegroundColor Yellow
}
else {
    Write-Host "  some components are still behind; a reboot or the next cumulative update is required." -ForegroundColor Yellow
}
