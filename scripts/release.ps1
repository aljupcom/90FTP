<#
.SYNOPSIS
    90FTP Automated Versioning, Building, Signing, GitHub Tag & Release Pipeline
.EXAMPLE
    .\scripts\release.ps1 -Version "2.6.0" -Message "Added AES-256-GCM vault and remote updater"
#>
param(
    [Parameter(Mandatory=$true)]
    [string]$Version,
    [Parameter(Mandatory=$false)]
    [string]$Message = "Release update",
    [Parameter(Mandatory=$false)]
    [string[]]$Added = @(),
    [Parameter(Mandatory=$false)]
    [string[]]$Changed = @(),
    [Parameter(Mandatory=$false)]
    [string[]]$Fixed = @(),
    [Parameter(Mandatory=$false)]
    [string]$Changelog = "",
    [Parameter(Mandatory=$false)]
    [string]$Token = $env:GITHUB_TOKEN
)

if (-not $Token) {
    $Token = $env:GH_TOKEN
}
if (-not $Token) {
    try {
        $originUrl = git remote get-url origin
        if ($originUrl -match 'https://([^:]+):?([^@]*)@github\.com') {
            if ($matches[1].StartsWith("ghp_")) {
                $Token = $matches[1]
            } elseif ($matches[2].StartsWith("ghp_")) {
                $Token = $matches[2]
            }
        }
    } catch {}
}

[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

Write-Host ">>> [1/8] Updating version to v$Version across project files..." -ForegroundColor Cyan

# 1. Cargo.toml
$cargoPath = Join-Path $Root "Cargo.toml"
$cargo = Get-Content $cargoPath -Raw
$cargoVer = $Version
if (($Version -split '\.').Count -gt 3) {
    $parts = $Version -split '\.'
    $cargoVer = "0.0." + $parts[-1]
}
$cargo = $cargo -replace '(?m)^version\s*=\s*"[^"]+"', ('version = "' + $cargoVer + '"')
Set-Content $cargoPath $cargo -NoNewline

# 2. installer.iss
$issPath = Join-Path $Root "installer.iss"
$iss = Get-Content $issPath -Raw
$iss = $iss -replace '#define MyAppVersion\s+"[^"]+"', ('#define MyAppVersion "' + $Version + '"')
Set-Content $issPath $iss -NoNewline

# 3. src/ui/index.html
$htmlPath = Join-Path $Root "src\ui\index.html"
$html = Get-Content $htmlPath -Raw
$html = $html -replace '(?:const|let)\s+APP_CURRENT_VERSION\s*=\s*''[^'']+''', ('let APP_CURRENT_VERSION = ''' + $Version + '''')
$html = $html -replace 'Version\s+[0-9\.]+\s+(Pro|Dev)\s+\(Windows\s+x64\)', ('Version ' + $Version + ' Dev (Windows x64)')
$html = $html -replace 'Current:\s+v[0-9\.]+', ('Current: v' + $Version)
$html = $html -replace 'Modern Site Manager v[0-9\.]+', ('Modern Site Manager v' + $Version)
Set-Content $htmlPath $html -NoNewline

Write-Host ">>> [2/8] Running test suite..." -ForegroundColor Cyan
cargo test --bin ninety_ftp

Write-Host ">>> [3/8] Building release binary..." -ForegroundColor Cyan
cargo build --release

Write-Host ">>> [4/8] Signing binaries (target\release\ninety_ftp.exe and 90FTP.exe)..." -ForegroundColor Cyan
Stop-Process -Name "90FTP", "ninety_ftp" -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600

$signtool = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe"
if (Test-Path $signtool) {
    try {
        & $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /tr "http://timestamp.digicert.com" /td SHA256 /d "90FTP Client" "target\release\ninety_ftp.exe"
    } catch {
        & $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /d "90FTP Client" "target\release\ninety_ftp.exe"
    }
    Copy-Item "target\release\ninety_ftp.exe" "90FTP.exe" -Force
    try {
        & $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /tr "http://timestamp.digicert.com" /td SHA256 /d "90FTP Client" "90FTP.exe"
    } catch {
        & $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /d "90FTP Client" "90FTP.exe"
    }
} else {
    Copy-Item "target\release\ninety_ftp.exe" "90FTP.exe" -Force
}

Write-Host ">>> [5/8] Compiling Inno Setup installer dynamically (Version: $Version)..." -ForegroundColor Cyan
& "C:\Program Files\Inno Setup 7\ISCC.exe" "/DMyAppVersion=$Version" installer.iss

$setupExe = Join-Path $Root "dist\90FTP-Setup-v$Version.exe"
Write-Host ">>> [6/8] Signing installer: $setupExe..." -ForegroundColor Cyan
if (Test-Path $signtool) {
    try {
        & $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /tr "http://timestamp.digicert.com" /td SHA256 /d "90FTP Setup" $setupExe
    } catch {
        & $signtool sign /f "assets\certs\aljailane.pfx" /p "password123" /fd SHA256 /d "90FTP Setup" $setupExe
    }
}
Copy-Item $setupExe (Join-Path $Root "90FTP-Setup-v$Version.exe") -Force

Write-Host ">>> [7/8] Committing and pushing branch & tag to Git..." -ForegroundColor Cyan
git add .
try {
    git commit -m "release: v$Version - $Message"
} catch {}
git push origin main
git tag -a "v$Version" -m "Release v${Version} - $Message" -f
git push origin "v$Version" -f

Write-Host ">>> [8/8] Creating GitHub Release & uploading installer asset..." -ForegroundColor Cyan
$repo = "aljupcom/90FTP"
$headers = @{
    "Authorization" = "token $Token"
    "Accept" = "application/vnd.github.v3+json"
    "User-Agent" = "90FTP-Release-Agent"
}

# Construct rich, categorized release changelog
$releaseNotes = ""
$emojiRocket = [char]::ConvertFromUtf32(0x1F680)
$emojiBolt = [char]::ConvertFromUtf32(0x26A1)
$emojiShield = [char]::ConvertFromUtf32(0x1F6E1)

if ($Changelog -and $Changelog.Trim().Length -gt 0) {
    $releaseNotes = $Changelog.Trim()
} elseif ($Added.Count -gt 0 -or $Changed.Count -gt 0 -or $Fixed.Count -gt 0) {
    $sb = [System.Text.StringBuilder]::new()
    [void]$sb.AppendLine("## What's New in v$Version")
    if ($Message -and $Message -ne "Release update") {
        [void]$sb.AppendLine("$Message`n")
    }
    if ($Added.Count -gt 0) {
        [void]$sb.AppendLine("### $emojiRocket Added")
        foreach ($item in $Added) {
            [void]$sb.AppendLine("- $item")
        }
        [void]$sb.AppendLine()
    }
    if ($Changed.Count -gt 0) {
        [void]$sb.AppendLine("### $emojiBolt Changed")
        foreach ($item in $Changed) {
            [void]$sb.AppendLine("- $item")
        }
        [void]$sb.AppendLine()
    }
    if ($Fixed.Count -gt 0) {
        [void]$sb.AppendLine("### $emojiShield Fixed")
        foreach ($item in $Fixed) {
            [void]$sb.AppendLine("- $item")
        }
        [void]$sb.AppendLine()
    }
    $releaseNotes = $sb.ToString().Trim()
} elseif ($Message -match '(?m)^(###|\*|-|Added|Changed|Fixed)') {
    $releaseNotes = "## What's New in v$Version`n`n$Message"
} else {
    # Auto-extract commits since previous tag to build structured changelog
    $prevTag = git describe --tags --abbrev=0 HEAD^ 2>$null
    $commits = @()
    if ($prevTag) {
        $commits = git log "$prevTag..HEAD" --pretty=format:"%s" 2>$null
    }
    $addedList = @()
    $changedList = @()
    $fixedList = @()

    foreach ($c in $commits) {
        if ($c -match '^(add|feat|new)[:\s]' -or $c -like '*add*' -or $c -like '*compress*' -or $c -like '*extract*') {
            $addedList += $c
        } elseif ($c -match '^(fix|bug|patch)[:\s]' -or $c -like '*fix*' -or $c -like '*resolve*') {
            $fixedList += $c
        } elseif ($c -notmatch '^release:') {
            $changedList += $c
        }
    }

    $sb = [System.Text.StringBuilder]::new()
    [void]$sb.AppendLine("## What's New in v$Version")
    [void]$sb.AppendLine("$Message`n")
    if ($addedList.Count -gt 0) {
        [void]$sb.AppendLine("### $emojiRocket Added")
        foreach ($it in $addedList) { [void]$sb.AppendLine("- $it") }
        [void]$sb.AppendLine()
    } else {
        [void]$sb.AppendLine("### $emojiRocket Added")
        [void]$sb.AppendLine("- $Message")
        [void]$sb.AppendLine()
    }
    if ($changedList.Count -gt 0) {
        [void]$sb.AppendLine("### $emojiBolt Changed")
        foreach ($it in $changedList) { [void]$sb.AppendLine("- $it") }
        [void]$sb.AppendLine()
    }
    if ($fixedList.Count -gt 0) {
        [void]$sb.AppendLine("### $emojiShield Fixed")
        foreach ($it in $fixedList) { [void]$sb.AppendLine("- $it") }
        [void]$sb.AppendLine()
    }
    $releaseNotes = $sb.ToString().Trim()
}

Write-Host "`n--- Generated Release Changelog ---" -ForegroundColor Yellow
Write-Host $releaseNotes -ForegroundColor Gray
Write-Host "-----------------------------------`n" -ForegroundColor Yellow

$payloadObj = @{
    tag_name = "v$Version"
    target_commitish = "main"
    name = "90FTP v$Version Dev - $Message"
    body = $releaseNotes
    draft = $false
    prerelease = $false
    make_latest = "true"
}

$payloadJson = $payloadObj | ConvertTo-Json
$payloadBytes = [System.Text.Encoding]::UTF8.GetBytes($payloadJson)

try {
    try {
        $legacy = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/tags/v2.6.0" -Headers $headers -ErrorAction SilentlyContinue
        if ($legacy -and $legacy.id) {
            Write-Host "Found legacy v2.6.0 release (ID $($legacy.id)). Cleaning up..." -ForegroundColor Yellow
            Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/$($legacy.id)" -Method Delete -Headers $headers
        }
    } catch {}

    try {
        $existing = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/tags/v$Version" -Headers $headers -ErrorAction SilentlyContinue
        if ($existing -and $existing.id) {
            Write-Host "Existing release v$Version found (ID $($existing.id)). Deleting old release..." -ForegroundColor Yellow
            Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/$($existing.id)" -Method Delete -Headers $headers
        }
    } catch {}

    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases" -Method Post -Headers $headers -Body $payloadBytes -ContentType "application/json; charset=utf-8"
    $releaseId = $release.id
    Write-Host "Release created: ID $releaseId" -ForegroundColor Green

    $assetName = "90FTP-Setup-v$Version.exe"
    $uploadUri = "https://uploads.github.com/repos/$repo/releases/$releaseId/assets?name=$assetName"
    $uploadHeaders = @{
        "Authorization" = "token $Token"
        "Accept" = "application/vnd.github.v3+json"
        "User-Agent" = "90FTP-Release-Agent"
        "Content-Type" = "application/octet-stream"
    }

    try {
        $assetResp = Invoke-RestMethod -Uri $uploadUri -Method Post -Headers $uploadHeaders -InFile $setupExe
    } catch {
        $fileBytes = [System.IO.File]::ReadAllBytes($setupExe)
        $assetResp = Invoke-RestMethod -Uri $uploadUri -Method Post -Headers $uploadHeaders -Body $fileBytes
    }
    Write-Host "Asset uploaded: $($assetResp.browser_download_url)" -ForegroundColor Green
} catch {
    Write-Warning "GitHub Release API call: $_"
}

Write-Host ">>> SUCCESS: Released 90FTP v$Version!" -ForegroundColor Green
