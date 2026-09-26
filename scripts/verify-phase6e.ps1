#requires -Version 5.1
<#
.SYNOPSIS
    Phase 6-E.3 automated validation for photo-publisher.

.DESCRIPTION
    Runs a deterministic sequence of local checks and exits with a non-zero
    code at the first failure. Works on Windows PowerShell 5.1 and PowerShell
    7 (pwsh). Requires: git, cargo. Requires NO real credentials: the fixture
    uses the local-only v1 project contract, so no GitHub/R2/Vercel secrets
    are needed. The script never prints tokens or secret material.

    Exit codes: 0 = all checks passed, 1 = some check failed.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Continue'

# ---------------------------------------------------------------------------
# Harness
# ---------------------------------------------------------------------------

$script:Failed = $false
$script:CurrentStep = ''
$script:StepOutput = ''

function Write-StepHeader([string] $Name) {
    $script:CurrentStep = $Name
    $script:StepOutput = ''
    Write-Host "----- $Name"
}

function Complete-Step([bool] $Ok, [string] $Detail) {
    if ($Ok) {
        Write-Host "[PASS] $script:CurrentStep"
    } else {
        $script:Failed = $true
        Write-Host "[FAIL] $script:CurrentStep"
        if ($Detail) { Write-Host "       $Detail" }
        if ($script:StepOutput) {
            Write-Host "       --- captured output (last lines) ---"
            ($script:StepOutput -split "`r?`n" | Select-Object -Last 30) |
                ForEach-Object { Write-Host "       $_" }
        }
        Write-Host 'RESULT: FAIL'
        exit 1
    }
}

# Runs a native command inside the repository root. Never throws on non-zero
# exit codes; the caller decides.
function Invoke-Native([string] $File, [string[]] $Arguments) {
    $output = & $File @Arguments 2>&1 | Out-String
    $code = $LASTEXITCODE
    return @{ Code = $code; Text = $output }
}

function Assert-NativeOk([hashtable] $Result, [string] $CommandLine) {
    if ($Result.Code -ne 0) {
        $script:StepOutput = $Result.Text
        Complete-Step $false "command failed (exit $($Result.Code)): $CommandLine"
    }
}

# ---------------------------------------------------------------------------
# Repository root
# ---------------------------------------------------------------------------

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $RepoRoot

# Remove any credential variables from this process so the validation is
# provably secret-free.
foreach ($key in @(
    'PHOTO_PUBLISHER_GITHUB_TOKEN',
    'PHOTO_PUBLISHER_R2_ACCESS_KEY_ID',
    'PHOTO_PUBLISHER_R2_SECRET_ACCESS_KEY',
    'PHOTO_PUBLISHER_VERCEL_TOKEN'
)) {
    Remove-Item "Env:$key" -ErrorAction SilentlyContinue
}

# ---------------------------------------------------------------------------
# Step 1. Git state
# ---------------------------------------------------------------------------

Write-StepHeader 'Git state'
$statusRes = Invoke-Native git @('status', '--porcelain')
Assert-NativeOk $statusRes 'git status --porcelain'
if ($statusRes.Text.Trim() -ne '') {
    # A dirty tree is tolerated for local pre-commit runs; CI checkouts are
    # always clean. The frozen-file step still verifies the tag baseline.
    Write-Host '       note: working tree has local changes (allowed)'
}
$headRes = Invoke-Native git @('rev-parse', 'HEAD')
Assert-NativeOk $headRes 'git rev-parse HEAD'
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 2. Format
# ---------------------------------------------------------------------------

Write-StepHeader 'Format (cargo fmt --check)'
$fmt = Invoke-Native cargo @('fmt', '--all', '--', '--check')
Assert-NativeOk $fmt 'cargo fmt --all -- --check'
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 3. Workspace tests
# ---------------------------------------------------------------------------

Write-StepHeader 'Workspace tests (cargo test --workspace)'
$tests = Invoke-Native cargo @('test', '--workspace')
Assert-NativeOk $tests 'cargo test --workspace'
if ($tests.Text -notmatch 'test result: ok') {
    $script:StepOutput = $tests.Text
    Complete-Step $false 'no successful test summary found'
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 4. Clippy
# ---------------------------------------------------------------------------

Write-StepHeader 'Clippy (cargo clippy --workspace --all-targets -- -D warnings)'
$clippy = Invoke-Native cargo @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
Assert-NativeOk $clippy 'cargo clippy --workspace --all-targets -- -D warnings'
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 5. CLI build + fixture
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI build (cargo build --release --locked)'
$build = Invoke-Native cargo @('build', '--release', '--locked')
Assert-NativeOk $build 'cargo build --release --locked'
$exe = Join-Path $RepoRoot 'target\release\photo-publisher.exe'
if (-not (Test-Path $exe)) {
    Complete-Step $false "executable not found: $exe"
}
Complete-Step $true ''

# Local-only fixture: a v1 project with one tiny JPEG. No providers, no
# credentials, no remote targets.
$work = Join-Path $env:TEMP 'photo-publisher-verify-6e'
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Path (Join-Path $work 'fotos') -Force | Out-Null

$projectJson = @'
{
  "schemaVersion": 1,
  "project": {"id": "verify-6e", "name": "Verify 6E"},
  "gallery": {"template": "local", "title": "Verify 6E"},
  "source": {"type": "folder", "path": "fotos"},
  "repository": {"provider": "local", "repository": "local"},
  "hosting": {"provider": "local"},
  "storage": {"preview": {"provider": "local"}, "highResolution": {"provider": "local"}}
}
'@
[IO.File]::WriteAllBytes(
    (Join-Path $work 'project.json'),
    (New-Object System.Text.UTF8Encoding($false)).GetBytes($projectJson)
)

# Deterministic 1x1 JPEG (JFIF). Embedded as base64 so the check needs no
# image tooling.
$jpegBase64 = '/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAABAAEDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDkaKKK8o/Sz//Z'
[IO.File]::WriteAllBytes(
    (Join-Path $work 'fotos\a.jpg'),
    [Convert]::FromBase64String($jpegBase64)
)

$projectPath = Join-Path $work 'project.json'

# Runs the CLI from inside the fixture directory and captures both streams
# completely separately.
function Invoke-Cli([string[]] $Arguments) {
    $outFile = Join-Path $env:TEMP 'verify-6e.stdout.txt'
    $errFile = Join-Path $env:TEMP 'verify-6e.stderr.txt'
    Push-Location $work
    try {
        & $exe @Arguments 1> $outFile 2> $errFile
        $code = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    $stdout = ''
    $stderr = ''
    if (Test-Path $outFile) { $stdout = [IO.File]::ReadAllText($outFile) }
    if (Test-Path $errFile) { $stderr = [IO.File]::ReadAllText($errFile) }
    return @{ Code = $code; StdOut = $stdout; StdErr = $stderr }
}

# ---------------------------------------------------------------------------
# Step 6. CLI validate (fixture)
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI validate (local fixture)'
$result = Invoke-Cli @('validate', 'project.json', '--json')
if ($result.Code -ne 0) {
    Complete-Step $false "validate exited $($result.Code): $($result.StdErr)"
}
$data = $result.StdOut | ConvertFrom-Json
if (-not ($data.ok -and $data.command -eq 'validate' -and $data.data.valid)) {
    Complete-Step $false "unexpected validate envelope: $($result.StdOut)"
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 7. --help
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI --help'
$result = Invoke-Cli @('--help')
if ($result.Code -ne 0 -or $result.StdOut -notmatch 'publish') {
    Complete-Step $false "--help failed (exit $($result.Code))"
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 8. --verbose (progress on stderr only)
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI --verbose'
$result = Invoke-Cli @('publish', 'project.json', '--verbose')
if ($result.Code -ne 0) {
    Complete-Step $false "publish --verbose exited $($result.Code): $($result.StdErr)"
}
if ($result.StdErr -notmatch 'result:') {
    Complete-Step $false '--verbose did not emit progress on stderr'
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 9. --silent
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI --silent'
$result = Invoke-Cli @('publish', 'project.json', '--silent')
if ($result.Code -ne 0) {
    Complete-Step $false "publish --silent exited $($result.Code)"
}
if ($result.StdOut.Trim() -ne '') {
    Complete-Step $false '--silent must keep stdout empty'
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 10. --json
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI --json'
$result = Invoke-Cli @('publish', 'project.json', '--json')
if ($result.Code -ne 0) {
    Complete-Step $false "publish --json exited $($result.Code)"
}
$data = $result.StdOut | ConvertFrom-Json
if (-not ($data.ok -and $data.command -eq 'publish' -and $data.data.published)) {
    Complete-Step $false "unexpected publish envelope: $($result.StdOut)"
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 11. --json --verbose stream separation
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI --json --verbose (JSON only on stdout, progress only on stderr)'
$result = Invoke-Cli @('publish', 'project.json', '--json', '--verbose')
if ($result.Code -ne 0) {
    Complete-Step $false "publish --json --verbose exited $($result.Code)"
}
# stdout must parse as one single JSON envelope and nothing else
$data = $result.StdOut | ConvertFrom-Json
if (-not ($data.ok -and $data.command -eq 'publish')) {
    Complete-Step $false 'stdout is not exactly the JSON envelope'
}
# stdout must not carry human text
if ($result.StdOut -match 'publish: ok') {
    Complete-Step $false 'stdout contains non-JSON progress text'
}
# stderr must never carry the JSON envelope
if ($result.StdErr -match '"ok"') {
    Complete-Step $false 'stderr contains JSON payload'
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 12. --dry-run
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI --dry-run'
$result = Invoke-Cli @('publish', 'project.json', '--dry-run', '--json')
if ($result.Code -ne 0) {
    Complete-Step $false "publish --dry-run exited $($result.Code)"
}
$data = $result.StdOut | ConvertFrom-Json
if (-not ($data.ok -and $data.data.dry_run -eq $true)) {
    Complete-Step $false "dry-run envelope mismatch: $($result.StdOut)"
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 13+14. Publish with no changes + idempotency
# ---------------------------------------------------------------------------

Write-StepHeader 'CLI publish without changes / idempotency'
$galleryPath = Join-Path $work 'output\gallery.json'
if (-not (Test-Path $galleryPath)) {
    Complete-Step $false "gallery was not produced at $galleryPath"
}
$galleryBytesFirst = [IO.File]::ReadAllBytes($galleryPath)
$first = Invoke-Cli @('publish', 'project.json', '--json')
if ($first.Code -ne 0) { Complete-Step $false "second publish exited $($first.Code)" }
$galleryBytesSecond = [IO.File]::ReadAllBytes($galleryPath)
if (-not [Linq.Enumerable]::SequenceEqual($galleryBytesFirst, $galleryBytesSecond)) {
    Complete-Step $false 're-publish changed gallery.json bytes (not idempotent)'
}
$data = $first.StdOut | ConvertFrom-Json
if (-not ($data.ok -and $data.data.published)) {
    Complete-Step $false 're-publish envelope mismatch'
}
$dryAfter = Invoke-Cli @('publish', 'project.json', '--dry-run', '--json')
$dataDry = $dryAfter.StdOut | ConvertFrom-Json
if ($dataDry.data.would_publish -ne $false) {
    Complete-Step $false 'dry-run reports changes after an unchanged publication'
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Step 15. Frozen files (unchanged since the v0.1.0 baseline)
# ---------------------------------------------------------------------------

Write-StepHeader 'Frozen files (vs tag v0.1.0)'
$tagRes = Invoke-Native git @('rev-list', '-n', '1', 'v0.1.0')
Assert-NativeOk $tagRes 'git rev-list -n 1 v0.1.0'
$tagSha = $tagRes.Text.Trim()
if ($tagSha -ne 'eef921aabd97e20146f49b77695c94647e630b9e') {
    Complete-Step $false "v0.1.0 points at $tagSha instead of eef921aabd97e20146f49b77695c94647e630b9e"
}
$frozenPaths = @(
    'schemas/',
    'crates/publisher-core/',
    'crates/publisher-pipeline/',
    'crates/provider-contracts/',
    'crates/provider-github/',
    'crates/provider-r2/',
    'crates/provider-vercel/',
    'crates/contract-validator/',
    '.github/workflows/release.yml'
)
$diffRes = Invoke-Native git (@('diff', '--quiet', 'v0.1.0', 'HEAD', '--') + $frozenPaths)
if ($diffRes.Code -ne 0) {
    $script:StepOutput = (Invoke-Native git (@('diff', '--stat', 'v0.1.0', 'HEAD', '--') + $frozenPaths)).Text
    Complete-Step $false 'frozen files differ from the v0.1.0 baseline (see stat above)'
}
Complete-Step $true ''

# ---------------------------------------------------------------------------
# Result
# ---------------------------------------------------------------------------

$global:LASTEXITCODE = 0
Write-Host 'RESULT: PASS'
exit 0
