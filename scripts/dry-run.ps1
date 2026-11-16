<#
.SYNOPSIS
  Shows what a check would change, and changes nothing.

.DESCRIPTION
  Without -Repo: 'tss check --dry-run' on the sandbox (the folders scripts\run.ps1 and scripts\tss.ps1 use).

  With -Repo: previews one repository from scratch. A throwaway settings folder and an empty skills folder are made
  in %TEMP%, the repository is added there, 'tss check --dry-run' lists what would be installed, and the throwaway
  folders are deleted. Nothing else is read or written; the real and sandbox folders are not touched.

.PARAMETER Repo
  OWNER/NAME, or a GitHub URL of one folder: https://github.com/OWNER/NAME/tree/BRANCH/FOLDER

.PARAMETER UseToken
  Send the saved GitHub token (or the GitHub CLI's): needed for private repositories.

.PARAMETER Release
  Use the release build instead of the debug build.

.EXAMPLE
  scripts\dry-run.ps1
  scripts\dry-run.ps1 -Repo AngelosP/tiny-skill-sync-fixtures-public
  scripts\dry-run.ps1 -Repo AngelosP/tiny-skill-sync-fixtures-private -UseToken
#>
param([string]$Repo = '', [switch]$UseToken, [switch]$Release)

. "$PSScriptRoot\common.ps1"
$build = 'debug'
$cargoArgs = @('build', '--quiet', '--bin', 'tss')
if ($Release) { $build = 'release'; $cargoArgs += '--release' }
Use-Cargo
Push-Location $RepoRoot
try { Invoke-Native cargo $cargoArgs } finally { Pop-Location }
$exe = Join-Path (Get-ExeDir $build) 'tss.exe'

function Invoke-Tss([string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    & $exe @Arguments | Out-Host
    return $LASTEXITCODE
}

if (-not $Repo) {
    $code = Invoke-InSandbox { Invoke-Tss @('check', '--dry-run') }
    exit $code
}

$root = Join-Path $env:TEMP ('tss-dryrun-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
try {
    $code = Invoke-InSandbox -Root $root -Body {
        $add = @('add', $Repo)
        if ($UseToken) { $add += '--use-token' }
        $added = Invoke-Tss $add
        if ($added -ne 0) { return $added }
        Invoke-Tss @('check', '--dry-run')
    }
} finally {
    if (Test-Path $root) { Remove-Item -Recurse -Force $root }
}
exit $code
