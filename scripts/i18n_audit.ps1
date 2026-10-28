# =============================================================================
# 90FTP i18n audit
# -----------------------------------------------------------------------------
# Guards the localization contract documented in .gemini/rules/language_policy.md
#
#   1. Arabic characters may appear ONLY inside src/ui/i18n.ar.js.
#   2. Every data-i18n* key used in index.html must exist in i18n.en.js.
#   3. Every t('key') / tn('base', ...) key used in app.js must exist in i18n.en.js.
#   4. No translation value may contain a build placeholder token.
#   5. i18n.en.js and i18n.ar.js must expose the same key set.
#
# Exit code 0 = clean, 1 = violations found.
# =============================================================================

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$ui   = Join-Path $root 'src\ui'

$failures = New-Object System.Collections.Generic.List[string]

function Read-Text([string]$path) {
    return [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)
}

function Get-DictKeys([string]$path) {
    $text = Read-Text $path
    $keys = New-Object System.Collections.Generic.HashSet[string]
    foreach ($m in [regex]::Matches($text, '(?m)^\s*"([^"]+)"\s*:')) {
        [void]$keys.Add($m.Groups[1].Value)
    }
    return $keys
}

$enPath = Join-Path $ui 'i18n.en.js'
$arPath = Join-Path $ui 'i18n.ar.js'
$enKeys = Get-DictKeys $enPath
$arKeys = Get-DictKeys $arPath

# --- 1. Arabic must stay inside i18n.ar.js --------------------------------
$arabic = [regex]'[\u0600-\u06FF]'
$scan = @(
    (Join-Path $ui 'index.html'),
    (Join-Path $ui 'app.js'),
    (Join-Path $ui 'terminal.html'),
    (Join-Path $ui 'i18n.js'),
    (Join-Path $ui 'i18n.en.js')
)
$scan += Get-ChildItem -Path (Join-Path $root 'src') -Filter *.rs -Recurse -File | ForEach-Object { $_.FullName }

foreach ($file in $scan) {
    if (-not (Test-Path $file)) { continue }
    $text = Read-Text $file
    if ($arabic.IsMatch($text)) {
        $failures.Add("Arabic characters found outside i18n.ar.js: $file")
    }
}

# --- 2. data-i18n keys ----------------------------------------------------
$html = Read-Text (Join-Path $ui 'index.html')
$attrKeys = [regex]::Matches($html, 'data-i18n(?:-title|-placeholder|-aria-label)?="([^"]+)"')
foreach ($m in $attrKeys) {
    $key = $m.Groups[1].Value
    if (-not $enKeys.Contains($key)) {
        $failures.Add("index.html uses unknown key: $key")
    }
}

# --- 3. t() / tn() keys ---------------------------------------------------
$js = Read-Text (Join-Path $ui 'app.js')
$callKeys = [regex]::Matches($js, "\b(?:t|tn)\(\s*'([^']+)'")
foreach ($m in $callKeys) {
    $key = $m.Groups[1].Value
    # tn('count.files', n) resolves count.files.<form>; accept the plural base.
    if (-not $enKeys.Contains($key) -and -not $enKeys.Contains("$key.other")) {
        $failures.Add("app.js uses unknown key: $key")
    }
}

# --- 4. Placeholder tokens inside values ----------------------------------
$forbidden = @('__APP_JS__', '__I18N_EN__', '__I18N_AR__', '__I18N_JS__', '__XTERM_JS__', '__XTERM_CSS__', '__XTERM_FIT_JS__')
foreach ($dict in @($enPath, $arPath)) {
    $text = Read-Text $dict
    foreach ($token in $forbidden) {
        if ($text.Contains($token)) {
            $failures.Add("Placeholder token '$token' appears inside $dict")
        }
    }
}

# --- 5. Key parity --------------------------------------------------------
$missingInAr = @($enKeys | Where-Object { -not $arKeys.Contains($_) })
$extraInAr   = @($arKeys | Where-Object { -not $enKeys.Contains($_) })
if ($missingInAr.Count -gt 0) {
    $failures.Add("$($missingInAr.Count) key(s) missing from i18n.ar.js (first: $($missingInAr[0]))")
}
if ($extraInAr.Count -gt 0) {
    $failures.Add("$($extraInAr.Count) orphan key(s) in i18n.ar.js (first: $($extraInAr[0]))")
}

# --- Report ---------------------------------------------------------------
# Informational coverage metric: how many static English text nodes in
# index.html currently resolve to a dictionary entry through the reverse index.
$enText = Read-Text $enPath
$phrases = New-Object System.Collections.Generic.HashSet[string]
foreach ($m in [regex]::Matches($enText, '(?m)^\s*"([^"]+)"\s*:\s*"((?:[^"\\]|\\.)*)"')) {
    if ($m.Groups[1].Value.StartsWith('re.')) { continue }
    $value = $m.Groups[2].Value -replace '\\"', '"' -replace '\\\\', '\'
    [void]$phrases.Add(($value -replace '\s+', ' ').Trim())
}
$coverageHtml = $html -replace '(?s)<style.*?</style>', '' -replace '(?s)<script.*?</script>', ''
$covered = 0
$uncovered = 0
foreach ($m in [regex]::Matches($coverageHtml, '>([^<>]+)<')) {
    $text = ($m.Groups[1].Value -replace '\s+', ' ').Trim()
    if ($text.Length -eq 0 -or $text -notmatch '[A-Za-z]') { continue }
    if ($text.Length -gt 220) { continue }
    if ($phrases.Contains($text)) { $covered++ } else { $uncovered++ }
}

Write-Host ""
Write-Host "90FTP i18n audit" -ForegroundColor Cyan
Write-Host "  en keys : $($enKeys.Count)"
Write-Host "  ar keys : $($arKeys.Count)"
Write-Host "  data-i18n attributes: $($attrKeys.Count)"
Write-Host "  t()/tn() call sites : $($callKeys.Count)"
Write-Host "  static text coverage: $covered translated / $uncovered untranslated"
Write-Host ""

if ($failures.Count -eq 0) {
    Write-Host "PASS - localization contract is intact." -ForegroundColor Green
    exit 0
}

foreach ($f in $failures) {
    Write-Host "FAIL - $f" -ForegroundColor Red
}
exit 1
