<#
.SYNOPSIS
  Checks the code: formatting, clippy, then every test.

.DESCRIPTION
  The same checks a change must pass before it is done (see CONTRIBUTING.md).
  Tests work only in their own folders under %TEMP% (or under -Root); they never touch the real skills folder,
  the real settings or the saved GitHub token. A failing test may leave its folder behind: 'scripts\dev.ps1 clean'
  removes those.

.PARAMETER Quick
  Only run the tests: no formatting check and no clippy.

.PARAMETER Filter
  Only run tests whose name contains this text (passed to 'cargo test').

.PARAMETER Root
  Run the file-system tests in this folder instead of %TEMP%: for example a folder on another drive, a ReFS or
  Dev Drive volume, or a network share. Sets TSS_TEST_ROOT for the run.

.EXAMPLE
  scripts\test.ps1
  scripts\test.ps1 -Quick -Filter held
  scripts\test.ps1 -Root \\nas\share\tss-tests
#>
param(
    [switch]$Quick,
    [string]$Filter = '',
    [string]$Root = ''
)

. "$PSScriptRoot\common.ps1"
Use-Cargo
Push-Location $RepoRoot
$savedRoot = $env:TSS_TEST_ROOT
try {
    if (-not $Quick) {
        Write-Host '== cargo fmt --check' -ForegroundColor Cyan
        Invoke-Native cargo @('fmt', '--check')
        Write-Host '== cargo clippy' -ForegroundColor Cyan
        Invoke-Native cargo @('clippy', '--all-targets', '--', '-D', 'warnings')
    }
    if ($Root) {
        New-Item -ItemType Directory -Force $Root | Out-Null
        $env:TSS_TEST_ROOT = $Root
    }
    Write-Host '== cargo test' -ForegroundColor Cyan
    $testArgs = @('test', '--no-fail-fast')
    if ($Filter) { $testArgs += @('--', $Filter) }
    Invoke-Native cargo $testArgs
    Write-Host 'All checks passed.' -ForegroundColor Green
} finally {
    $env:TSS_TEST_ROOT = $savedRoot
    Pop-Location
    $left = @(Get-ChildItem $env:TEMP -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match $TestTempPattern })
    if ($left.Count -gt 0) {
        Write-Host "$($left.Count) test folder(s) were left in $env:TEMP. Remove them with: scripts\dev.ps1 clean" -ForegroundColor Yellow
    }
}
