<#
.SYNOPSIS
  Runs the command line (tss) on the sandbox: its own settings and skills folders under .tss-test\.

.DESCRIPTION
  Everything after the script name goes to tss unchanged. Builds first (a debug build unless -Release).
  Uses the same folders as scripts\run.ps1, so both see the same settings and skills.
  'token set' and 'token clear' are refused: the saved GitHub token is shared with the real app.
  The exit code is tss's: 0 all fine, 1 something needs attention, 2 failed, 3 bad command line.

.EXAMPLE
  scripts\tss.ps1 add AngelosP/tiny-skill-sync-fixtures-public
  scripts\tss.ps1 check --dry-run
  scripts\tss.ps1 -Release status
#>
# No [Parameter()] attributes on purpose: then options such as --dry-run stay in $args untouched.
param([switch]$Release)

. "$PSScriptRoot\common.ps1"
$tssArgs = @($args | ForEach-Object { "$_" })
Assert-NotTokenChange $tssArgs
Use-Cargo
Push-Location $RepoRoot
try {
    $build = 'debug'
    $cargoArgs = @('build', '--quiet', '--bin', 'tss')
    if ($Release) { $build = 'release'; $cargoArgs += '--release' }
    Invoke-Native cargo $cargoArgs
} finally {
    Pop-Location
}
$exe = Join-Path (Get-ExeDir $build) 'tss.exe'
$code = Invoke-InSandbox {
    $ErrorActionPreference = 'Continue'
    & $exe @tssArgs | Out-Host
    $LASTEXITCODE
}
exit $code
