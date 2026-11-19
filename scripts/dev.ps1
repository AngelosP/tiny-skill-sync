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
  e2e      Run the real app against real GitHub test repositories and check what it does. Options: see scripts\e2e.ps1
  soak     Leave the app running for an hour or so against a test repository and measure it. Options: see scripts\soak.ps1
  demo     Make the captioned README GIF with throwaway fixture data. Options: -Python PATH -SkipBuild
  clean    Close the sandbox copy and any copy a live script left running, delete the sandbox, test leftovers in %TEMP%,
           the test startup entries (HKCU\Software\tiny-skill-sync-test) and bench results. Keeps target\ (use
           'cargo clean' for that).
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
    'e2e'     = 'e2e.ps1'
    'soak'    = 'soak.ps1'
    'demo'    = 'demo.ps1'
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
        $leftovers = @(Get-ChildItem $env:TEMP -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match $TestTempPattern -or $_.Name -match $ScriptTempPattern })
        # A live script that was stopped half way can leave its app running on its folder: ask it to quit first.
        foreach ($d in $leftovers) {
            $exe = Join-Path $d.FullName 'bin\tiny-skill-sync.exe'
            if (Test-Path $exe) { Invoke-InSandbox -Root $d.FullName -Body { Start-Process $exe -ArgumentList '--quit' -Wait | Out-Null } }
        }
        foreach ($d in $leftovers) {
            for ($try = 1; ; $try++) {
                try { Remove-Item -Recurse -Force $d.FullName -ErrorAction Stop; break }
                catch {
                    # Windows PowerShell cannot delete a path longer than 260 characters (e2e's signin scenario makes
                    # some); the \\?\ form can.
                    try { [IO.Directory]::Delete('\\?\' + $d.FullName, $true); break } catch { }
                    if ($try -ge 10) { throw }; Start-Sleep -Milliseconds 300
                }
            }
            $gone += $d.FullName
        }
        Get-ChildItem $env:TEMP -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match $ScriptTempFilePattern } |
            ForEach-Object { Remove-Item -Force $_.FullName; $gone += $_.FullName }
        # The startup entries of tests and scripts: never Windows' own Run key.
        if (Test-Path $TestRegistryRoot) { Remove-Item -Recurse -Force $TestRegistryRoot; $gone += $TestRegistryRoot }
        if ($gone.Count -eq 0) { 'Nothing to clean.' } else { $gone | ForEach-Object { "Deleted $_" } }
    }
    { $_ -in 'help', '/?' } {
        Get-Help $PSCommandPath -Detailed | Out-Host
    }
    default {
        Write-Host "Unknown task '$Task'. Tasks: run, build, test, tss, dry-run, bench, e2e, soak, demo, clean, help" -ForegroundColor Red
        exit 3
    }
}
