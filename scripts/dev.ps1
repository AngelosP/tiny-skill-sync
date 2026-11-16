<#
.SYNOPSIS
  The one script to start from. Runs a task; the default is 'run'.

.DESCRIPTION
  scripts\dev.ps1 [TASK] [options of that task]

  run      Build (debug) and start the tray app on the sandbox (.tss-test\). Options: -Release -Fresh -Quit
  build    Build both executables and print their sizes. Options: -DebugBuild
  test     Formatting, clippy and every test. Options: -Quick -Filter TEXT -Root FOLDER
  tss      Run the command line on the sandbox: scripts\dev.ps1 tss status
  dry-run  Show what a check would change. Options: -Repo OWNER/NAME -UseToken -Release
  bench    Measure startup, memory, CPU and disk use against the budgets. Options: see scripts\bench.ps1
  clean    Close the sandbox copy, delete the sandbox, test leftovers in %TEMP% and bench results.
           Keeps target\ (use 'cargo clean' for that).
  help     This text.

  Nothing here touches the real skills folder, the real settings or the saved GitHub token.

.EXAMPLE
  scripts\dev.ps1
  scripts\dev.ps1 test -Quick
  scripts\dev.ps1 tss check --dry-run
  scripts\dev.ps1 dry-run -Repo AngelosP/tiny-skill-sync-fixtures-public
#>
# No [Parameter()] attributes on purpose: the task's own options stay in $args and are passed on as they are.
param([string]$Task = 'run')

. "$PSScriptRoot\common.ps1"
$taskArgs = $args

$scripts = @{
    'run'     = 'run.ps1'
    'build'   = 'build.ps1'
    'test'    = 'test.ps1'
    'tss'     = 'tss.ps1'
    'dry-run' = 'dry-run.ps1'
    'bench'   = 'bench.ps1'
}

switch ($Task) {
    { $scripts.ContainsKey($_) } {
        & (Join-Path $PSScriptRoot $scripts[$Task]) @taskArgs
        exit $LASTEXITCODE
    }
    'clean' {
        & "$PSScriptRoot\run.ps1" -Quit
        $gone = @()
        if (Test-Path $Sandbox) { Remove-Item -Recurse -Force $Sandbox; $gone += $Sandbox }
        $benchOut = Join-Path $RepoRoot 'bench\out'
        if (Test-Path $benchOut) { Remove-Item -Recurse -Force $benchOut; $gone += $benchOut }
        Get-ChildItem $env:TEMP -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match $TestTempPattern -or $_.Name -match $ScriptTempPattern } |
            ForEach-Object { Remove-Item -Recurse -Force $_.FullName; $gone += $_.FullName }
        if ($gone.Count -eq 0) { 'Nothing to clean.' } else { $gone | ForEach-Object { "Deleted $_" } }
    }
    { $_ -in 'help', '/?' } {
        Get-Help $PSCommandPath -Detailed | Out-Host
    }
    default {
        Write-Host "Unknown task '$Task'. Tasks: run, build, test, tss, dry-run, bench, clean, help" -ForegroundColor Red
        exit 3
    }
}
