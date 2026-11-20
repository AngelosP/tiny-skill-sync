<#
.SYNOPSIS
  Checks the real CLI and Settings page offline, using a fresh copy of the locally built release pair.

.DESCRIPTION
  Build with scripts\dev.ps1 build first. No repositories, gh, saved token, release requests, downloads or
  startup changes. Only the test window receives messages. Both manual buttons run with the app-update isolation guard set.

.PARAMETER Screenshot
  Save this copy's Settings window in bench\out for visual review. Never capture the desktop.
#>
[CmdletBinding()]
param([switch]$Screenshot)

. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\e2e-lib.ps1"

function Run-Tss([string[]]$TssArgs) {
    $r = Invoke-LiveTss $root $TssArgs $child
    if ($r.Code -ne 0) { throw "tss $($TssArgs -join ' ') failed (exit $($r.Code))." }
    $r.Text
}

function File-Stamp([string]$Path) {
    '{0}:{1}' -f (Get-Item -LiteralPath $Path).LastWriteTimeUtc.Ticks, (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
}

function Has-Prefs([string]$File, [bool]$Enabled, [string]$Interval, [string]$Timing) {
    try { $p = ([IO.File]::ReadAllText($File) | ConvertFrom-Json).app_update } catch { return $false }
    $p.enabled -is [bool] -and $p.enabled -eq $Enabled -and $p.interval -ceq $Interval -and $p.timing -ceq $Timing
}

function Wait-ForSave([string]$Interval, [string]$Timing) {
    # The worker can replace the file a little before the window is ready for another edit.
    [void](Wait-Live {
        (Has-Prefs $shared $true $Interval $Timing) -and (Read-Hero $win).Sub -notlike 'Saving *'
    } "Settings to finish saving $Interval / $Timing" 10 100)
}

$tray = $null
$root = New-LiveRoot 'e2e'
try {
    $child = Get-AnonymousAppEnv $root
    $child.TSS_API_BASE = 'http://127.0.0.1:1'
    $child.TSS_RAW_BASE = $child.TSS_API_BASE
    $marks = Join-Path $root 'out\marks.txt'
    $child.TSS_BENCH_REPORT = $marks
    $bin = Join-Path $root 'bin'
    $cli = Join-Path $bin 'tss.exe'
    $gui = Join-Path $bin 'tiny-skill-sync.exe'
    $cliStamp = File-Stamp $cli
    $guiStamp = File-Stamp $gui

    $status = Run-Tss @('update', 'status')
    if ($status -notmatch 'Automatic app updates: on\b' -or $status -notmatch 'interval: 1 day\b' -or $status -notmatch 'timing: immediate\b' -or
        $status -notmatch 'Installed on disk \(tray and tss\): \d+\.\d+\.\d+') { throw 'Unexpected app-update defaults or installed version.' }
    [void](Run-Tss @('update', 'off'))
    [void](Run-Tss @('update', 'every', '7d'))
    [void](Run-Tss @('update', 'when', 'quit'))
    $local = Join-Path $root 'home\config.json'
    if (-not (Has-Prefs $local $false 'weekly' 'on_quit')) { throw 'CLI preferences did not reach local config.json.' }
    $localStamp = File-Stamp $local
    [void](Run-Tss @('portable'))
    $shared = Join-Path $bin 'tiny-skill-sync.settings.json'
    $sources = Join-Path $bin 'tiny-skill-sync.sources.json'
    $sourcesStamp = File-Stamp $sources
    if (-not (Has-Prefs $shared $false 'weekly' 'on_quit')) { throw 'Portable export lost the local preferences.' }
    [void](Run-Tss @('update', 'every', '30d'))
    if (-not (Has-Prefs $shared $false 'monthly' 'on_quit')) { throw 'CLI update did not reach the portable settings.' }
    Write-Host 'PASS: CLI defaults, local preference edits and portable export.'

    $tray = Start-LiveTray $root @() $child
    [void](Wait-Live { (Get-MarkCount $marks 'job-startup') -gt 0 } 'the isolated tray to finish startup' 30 100)
    $win = Open-Window $tray
    Click-Ui $win $UiPanel.Nav $UiId.NavSettings
    $page = $UiPanel.Page
    [void](Wait-Live { (Read-Ui $win $page $UiId.AppHeading) -eq 'App updates' } 'Settings to show App updates' 10 100)
    # Scroll only this panel (WM_VSCROLL, SB_BOTTOM), without moving the mouse, using the keyboard or taking focus.
    [void][TssLive]::SendMessageW([TssLive]::GetDlgItem($win, $page), 0x115, [IntPtr]7, [IntPtr]::Zero)
    if ((Read-Ui $win $page $UiId.AppCheck) -ne 'Check only' -or (Read-Ui $win $page $UiId.AppInstall) -ne 'Install now' -or
        (Read-Ui $win $page $UiId.AppAuto) -ne 'Automatic app updates' -or (Get-Checked $win $page $UiId.AppAuto) -or
        (Read-Ui $win $page $UiId.AppEvery) -ne "App update check interval`nEvery 30 days" -or
        (Read-Ui $win $page $UiId.AppWhen) -ne "When to install app updates`nOn Quit") {
        throw 'Settings did not show the release actions and the saved portable preferences.'
    }

    Click-Ui $win $page $UiId.AppAuto
    Wait-ForSave 'monthly' 'on_quit'
    Choose-Ui $win $page $UiId.AppEvery 'Every 7 days'
    Wait-ForSave 'weekly' 'on_quit'
    Choose-Ui $win $page $UiId.AppWhen 'Immediate'
    Wait-ForSave 'weekly' 'immediate'
    if ($Screenshot) {
        $outDir = Join-Path $RepoRoot 'bench\out'
        New-Item -ItemType Directory -Force $outDir | Out-Null
        $file = Join-Path $outDir ('app-updates-' + (Split-Path $root -Leaf) + '.png')
        [void][TssLive]::Capture($win, $file)
        Write-Host "Settings window: $file"
    }
    foreach ($button in @($UiId.AppCheck, $UiId.AppInstall)) {
        $count = Get-MarkCount $marks 'job-app-update'
        Click-Ui $win $page $button
        [void](Wait-Live { (Get-MarkCount $marks 'job-app-update') -gt $count } 'the guarded manual app action' 10 100)
    }
    if ((Get-MarkCount $marks 'job-check') -ne 0) { throw 'An app button triggered a skill check.' }
    [void][TssLive]::SendMessageW($win, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
    if ($tray.HasExited -or (Get-MarkCount $marks 'quit') -ne 0) { throw 'Closing Settings must not quit the tray.' }
    Write-Host 'PASS: both manual buttons dispatched app-only jobs under the network/install guard; Settings close did not quit.'
    Send-LiveCommand $tray $CMD_QUIT
    if (-not $tray.WaitForExit(8000) -or $tray.ExitCode -ne 0) { throw 'The test tray did not quit cleanly.' }
    $status = Run-Tss @('update', 'status')
    if ($status -notmatch 'Automatic app updates: on\b' -or $status -notmatch 'interval: 7 days\b' -or $status -notmatch 'timing: immediate\b') {
        throw 'A fresh CLI process did not read back the Settings choices.'
    }
    if ((File-Stamp $sources) -ne $sourcesStamp -or (File-Stamp $local) -ne $localStamp -or
        (File-Stamp $cli) -ne $cliStamp -or (File-Stamp $gui) -ne $guiStamp) { throw 'A preference edit changed an unrelated file.' }
    Write-Host 'PASS: Settings edits survived Quit; both executables, local config and shared sources stayed untouched.'
} finally {
    try { Stop-LiveTray $tray } finally {
        Remove-LiveRoot $root
        if (Test-Path -LiteralPath $root) { throw "Could not remove this run's folder: $root" }
    }
}
