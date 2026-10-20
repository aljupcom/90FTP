<#
.SYNOPSIS
    90FTP Automated Versioning, Building, Signing & Release Pipeline
.EXAMPLE
    .\scripts\release.ps1 -Version "2.6.0" -Message "Added AES-256-GCM vault and remote updater"
#>
param(
    [Parameter(Mandatory=$true)]
    [string]$Version,
    [Parameter(Mandatory=$false)]
    [string]$Message = "Release update"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

Write-Host ">>> [1/7] Updating version to v$Version across project files..." -ForegroundColor Cyan

# 1. Cargo.toml
$cargoPath = Join-Path $Root "Cargo.toml"
$cargo = Get-Content $cargoPath -Raw
$cargo = $cargo -replace '(?m)^version\s*=\s*"[^"]+"', ('version = "' + $Version + '"')
Set-Content $cargoPath $cargo -NoNewline

# 2. installer.iss
$issPath = Join-Path $Root "installer.iss"
$iss = Get-Content $issPath -Raw
$iss = $iss -replace '#define MyAppVersion\s+"[^"]+"', ('#define MyAppVersion "' + $Version + '"')
Set-Content $issPath $iss -NoNewline

# 3. src/ui/index.html
$htmlPath = Join-Path $Root "src\ui\index.html"
$html = Get-Content $htmlPath -Raw
$html = $html -replace 'const APP_CURRENT_VERSION\s*=\s*''[^'']+''', ('const APP_CURRENT_VERSION = ''' + $Version + '''')
$html = $html -replace 'Version\s+[0-9\.]+\s+Pro\s+\(Windows\s+x64\)', ('Version ' + $Version + ' Pro (Windows x64)')
$html = $html -replace 'Current:\s+v[0-9\.]+', ('Current: v' + $Version)
Set-Content $htmlPath $html -NoNewline

Write-Host ">>> [2/7] Running test suite..." -ForegroundColor Cyan
cargo test --bin ninety_ftp

Write-Host ">>> [3/7] Building release binary..." -ForegroundColor Cyan
cargo build --release

Write-Host ">>> [4/7] Copying and signing 90FTP.exe..." -ForegroundColor Cyan
Stop-Process -Name "90FTP" -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
Copy-Item "target\release\ninety_ftp.exe" "90FTP.exe" -Force

$signtool = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe"
& $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /tr "http://timestamp.digicert.com" /td SHA256 /d "90FTP Client" "90FTP.exe"

Write-Host ">>> [5/7] Compiling Inno Setup installer..." -ForegroundColor Cyan
& "C:\Program Files\Inno Setup 7\ISCC.exe" installer.iss

$setupExe = Join-Path $Root "dist\90FTP-Setup-v$Version.exe"
Write-Host ">>> [6/7] Signing installer: $setupExe..." -ForegroundColor Cyan
& $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /tr "http://timestamp.digicert.com" /td SHA256 /d "90FTP Setup" $setupExe

Write-Host ">>> [7/7] Committing and pushing to Git..." -ForegroundColor Cyan
git add .
git commit -m "release: v$Version - $Message"
git push origin main

Write-Host ">>> SUCCESS: Released 90FTP v$Version!" -ForegroundColor Green
