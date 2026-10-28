<#
.SYNOPSIS
    Installs the Thmanyah (ثمانية) font family for the current Windows user.

.DESCRIPTION
    The Thmanyah licence allows the font to be installed on your own devices but
    forbids redistributing it, so it is never bundled with 90FTP and never
    committed to the repository. This helper copies the files you downloaded from
    font.thmanyah.com into your per-user font store and registers them, which is
    the same thing the Explorer "Install" command does for a standard user.

    No administrator rights are required, and nothing is written outside your own
    profile.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\scripts\install-thmanyah-font.ps1 `
        -Source "C:\Users\me\Desktop\Apps\thmanyahfont"
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$Source
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not (Test-Path $Source)) {
    throw "Font folder not found: $Source"
}

$target = Join-Path $env:LOCALAPPDATA "Microsoft\Windows\Fonts"
if (-not (Test-Path $target)) {
    New-Item -ItemType Directory -Path $target -Force | Out-Null
}

$registryPath = "HKCU:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts"
if (-not (Test-Path $registryPath)) {
    New-Item -Path $registryPath -Force | Out-Null
}

# Only the weights the interface actually uses. Light and Black are included
# because the Arabic heading rules reach for them.
$wanted = @(
    @{ File = "thmanyahsans-Light.otf";            Label = "thmanyah sans Light (OpenType)" },
    @{ File = "thmanyahsans-Regular.otf";          Label = "thmanyah sans (OpenType)" },
    @{ File = "thmanyahsans-Medium.otf";           Label = "thmanyah sans Medium (OpenType)" },
    @{ File = "thmanyahsans-Bold.otf";             Label = "thmanyah sans Bold (OpenType)" },
    @{ File = "thmanyahsans-Black.otf";            Label = "thmanyah sans Black (OpenType)" },
    @{ File = "thmanyahserifdisplay-Regular.otf";  Label = "thmanyah serif display (OpenType)" },
    @{ File = "thmanyahserifdisplay-Medium.otf";   Label = "thmanyah serif display Medium (OpenType)" },
    @{ File = "thmanyahserifdisplay-Bold.otf";     Label = "thmanyah serif display Bold (OpenType)" }
)

Write-Host ""
Write-Host "Installing the Thmanyah font for $env:USERNAME" -ForegroundColor Cyan
Write-Host ("=" * 58) -ForegroundColor DarkGray

$installed = 0
$missing = @()

foreach ($item in $wanted) {
    $candidate = Get-ChildItem -Path $Source -Filter $item.File -Recurse -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $candidate) {
        $missing += $item.File
        continue
    }

    $destination = Join-Path $target $item.File
    Copy-Item -Path $candidate.FullName -Destination $destination -Force
    New-ItemProperty -Path $registryPath -Name $item.Label -Value $destination `
        -PropertyType String -Force | Out-Null
    Write-Host ("  {0,-32} installed" -f $item.File) -ForegroundColor Green
    $installed++
}

if ($missing.Count -gt 0) {
    Write-Host ""
    Write-Host "Not found under $Source :" -ForegroundColor Yellow
    foreach ($name in $missing) { Write-Host "  - $name" -ForegroundColor Yellow }
    Write-Host "Download the package from font.thmanyah.com and extract it first."
}

# Font-change broadcast so already-running applications pick the family up
# without a sign-out.
if ($installed -gt 0) {
    Add-Type -Namespace Win32 -Name NativeMethods -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Auto, SetLastError = true)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam,
    IntPtr lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
    $HWND_BROADCAST = [IntPtr]0xffff
    $WM_FONTCHANGE = 0x001D
    $result = [UIntPtr]::Zero
    [void][Win32.NativeMethods]::SendMessageTimeout($HWND_BROADCAST, $WM_FONTCHANGE,
        [UIntPtr]::Zero, [IntPtr]::Zero, 2, 1000, [ref]$result)
}

Write-Host ""
Write-Host ("Done. {0} face(s) installed for the current user." -f $installed) -ForegroundColor Cyan
Write-Host "Restart 90FTP so the interface picks the new family up." -ForegroundColor DarkGray
