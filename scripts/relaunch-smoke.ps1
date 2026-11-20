<#
.SYNOPSIS
  Checks offline that the tray restarts itself after an app update: the old process starts the new one and quits.

.DESCRIPTION
  Builds the debug pair (cargo build --bins) unless -NoBuild, and runs a fresh copy of it from a throwaway folder in
  %TEMP% with its own TSS_HOME and TSS_SKILLS_DIR. The debug build's test commands (src\win\ui\shell.rs,
  CMD_TEST_RESTART) make the tray act as if it had installed the build in its own folder, without downloading or
  replacing anything, and restart into a held copy whose hash was checked, or refuse one with a wrong hash.
  Checks the handoff with the window closed and open (the reopened window says it restarted), a refused restart, a
  sign-out that holds the restart until Windows calls it off, that Quit stays quit, that a sign-out, a Quit or a close
  that arrives while the old tray checks the new one stops the new one (after a sign-out the old tray keeps running and
  says why), and that a release build in target\release, if there is one, ignores the test command. Windows' sign-out
  messages are sent or posted to the tray by this script, so no real sign-out takes place. No repositories, gh, saved
  token, release requests or startup changes. Only this run's windows receive messages; leftover trays are found by
  their folder and stopped by process id.

.PARAMETER NoBuild
  Use the debug pair in target\debug as it is.
#>
[CmdletBinding()]
param([switch]$NoBuild)

. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\e2e-lib.ps1"

$CMD_TEST_RESTART = 1098
$CMD_TEST_RESTART_WRONG = 1099
$WM_QUERYENDSESSION = 0x11
$WM_ENDSESSION = 0x16

# Every tray running from this folder's copy, whoever started it. A restarted tray is not a process this script started.
function Get-Trays([string]$Root) {
    $bin = (Join-Path $Root 'bin') + '\'
    @(Get-CimInstance Win32_Process -Filter "Name='tiny-skill-sync.exe'" |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($bin, [StringComparison]::OrdinalIgnoreCase) })
}

# A process this script did not start. Holding its handle keeps its exit code readable after it ends.
function Get-Tray([int]$ProcId) {
    $p = Get-Process -Id $ProcId
    [void]$p.Handle
    $p
}

function Wait-Exit([Diagnostics.Process]$P, [string]$What, [int]$TimeoutSec = 15) {
    if (-not $P.WaitForExit($TimeoutSec * 1000)) { throw "$What (pid $($P.Id)) did not exit within $TimeoutSec s." }
    $P.WaitForExit()
    if ($P.ExitCode -ne 0) { throw "$What (pid $($P.Id)) exited with code $($P.ExitCode), not 0." }
}

function Get-Title([int]$ProcId) {
    $h = Find-LiveWindow $ProcId $TRAY_CLASS
    if ($h -eq [IntPtr]::Zero) { throw "The tray (pid $ProcId) has no main window." }
    [TssLive]::Text($h)
}

function Ascii([string]$Text) { ($Text -replace '\s+', ' ') -replace '[^\x20-\x7E]', '?' }

# Fails if a tray other than pid $Keep runs from $Root at any look within $Seconds (one every 100 ms or so).
function Assert-NoOtherTray([string]$Root, [int]$Keep, [double]$Seconds, [string]$What) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    do {
        $other = @(Get-Trays $Root | Where-Object { $_.ProcessId -ne $Keep })
        if ($other.Count) { throw "${What}: pids $(($other | ForEach-Object { $_.ProcessId }) -join ', ')." }
        Start-Sleep -Milliseconds 100
    } while ($sw.Elapsed.TotalSeconds -lt $Seconds)
}

# Starts a tray on this run's folder and waits until it has finished starting.
function Start-Tray {
    $n = Get-MarkCount $marks 'job-startup'
    $p = Start-LiveTray $root @() $child
    [void](Wait-Live { (Get-MarkCount $marks 'job-startup') -gt $n } 'the tray to finish starting' 30 100)
    $p
}

# Sends the test install (or runs $Start) and follows the handoff: the new tray must be the same program, started by
# the old one with $Flag, on the same app folder, and be the only one left. Returns it and how long each step took.
function Invoke-Handoff([Diagnostics.Process]$Old, [string]$Flag, [scriptblock]$Start = { Send-LiveCommand $Old $CMD_TEST_RESTART }) {
    $title = Get-Title $Old.Id
    if ($title -cne $expectedTitle) { throw "The tray's title is '$title', not '$expectedTitle' (another app folder?)." }
    $trays = Get-MarkCount $marks 'tray'
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $Start
    $found = @(Wait-Live { @(Get-Trays $root | Where-Object { $_.ProcessId -ne $Old.Id }) } 'a new tray process' 30 100)
    $startMs = $sw.ElapsedMilliseconds
    if ($found.Count -ne 1) { throw "Expected one new tray, found $($found.Count)." }
    $c = $found[0]
    try { $new = Get-Tray $c.ProcessId } catch { throw "The new tray (pid $($c.ProcessId)) ended at once." }
    if ($c.ParentProcessId -ne $Old.Id) { throw "The new tray was started by pid $($c.ParentProcessId), not by the old tray ($($Old.Id))." }
    if ($c.ExecutablePath -ne $gui) { throw "The new tray runs $($c.ExecutablePath), not $gui." }
    if ($c.CommandLine.TrimEnd() -notmatch ('\s' + [regex]::Escape($Flag) + '$')) { throw "Expected only $Flag, got: $($c.CommandLine)" }
    Wait-Exit $Old 'The old tray'
    $quitMs = $sw.ElapsedMilliseconds
    [void](Wait-Live { (Get-MarkCount $marks 'tray') -gt $trays } 'the new tray to start running' 30 100)
    $runMs = $sw.ElapsedMilliseconds
    if ($new.HasExited) { throw "The new tray (pid $($new.Id)) exited with code $($new.ExitCode)." }
    $newTitle = Get-Title $new.Id
    if ($newTitle -cne $title) { throw "The new tray's title is '$newTitle', not '$title'." }
    $all = @(Get-Trays $root)
    if ($all.Count -ne 1 -or $all[0].ProcessId -ne $new.Id) { throw "Expected only the new tray, found pids $(($all | ForEach-Object { $_.ProcessId }) -join ', ')." }
    [pscustomobject]@{
        P = $new
        Text = "old pid $($Old.Id) started pid $($new.Id) $Flag after $startMs ms, exited 0 at $quitMs ms; the new tray ran at $runMs ms with the same title"
    }
}

# Sends the test install and, once the new tray has started and waits for the old one to go, posts $Msg to the old
# tray. The old tray, still checking the new one or quitting for it, must quit with 0 and stop the new one before it runs.
function Invoke-Interrupted([Diagnostics.Process]$Old, [int]$Msg, [int]$Wp, [string]$What) {
    $trays = Get-MarkCount $marks 'tray'
    $main = Find-LiveWindow $Old.Id $TRAY_CLASS
    if ($main -eq [IntPtr]::Zero) { throw "The tray (pid $($Old.Id)) has no main window." }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Send-LiveCommand $Old $CMD_TEST_RESTART
    $found = @(Wait-Live { @(Get-Trays $root | Where-Object { $_.ProcessId -ne $Old.Id }) } 'a new tray process' 30 20)
    $seenMs = $sw.ElapsedMilliseconds
    if (-not [TssLive]::PostMessageW($main, $Msg, [IntPtr]$Wp, [IntPtr]::Zero)) { throw "$What came too late: the old tray's window was gone." }
    $postMs = $sw.ElapsedMilliseconds
    try { $new = Get-Tray $found[0].ProcessId } catch { $new = $null }
    Wait-Exit $Old 'The old tray'
    $stopped = 'ended before it could be watched'
    if ($new) {
        if (-not $new.WaitForExit(5000)) { throw "The new tray (pid $($new.Id)) still runs after $What." }
        $new.WaitForExit()
        if ($new.ExitCode -ne 1) { throw "The new tray (pid $($new.Id)) exited with code $($new.ExitCode), not 1 (stopped)." }
        $stopped = 'was stopped (exit code 1)'
    }
    Assert-NoOtherTray $root 0 3 "A tray runs after $What during the restart"
    if ((Get-MarkCount $marks 'tray') -ne $trays) { throw "A tray started after $What during the restart." }
    "the new pid $($found[0].ProcessId) showed after $seenMs ms and $What was posted at $postMs ms; the old tray exited 0, the new one $stopped before it ran, and no tray ran in the 3 s after"
}

# Opens Settings and returns the skills folder it shows, then the last app update result, once that matches.
function Read-Settings([IntPtr]$Win, [string]$Pattern, [string]$What) {
    Click-Ui $Win $UiPanel.Nav $UiId.NavSettings
    $folder = Wait-Live { Read-Ui $Win $UiPanel.Page $UiId.SkillsDir } 'Settings to show the skills folder' 10 100
    if ($folder -ne $skills) { throw "Settings shows the skills folder '$folder', not '$skills'." }
    $script:shown = $null
    try {
        Wait-Live {
            # Scroll only this panel to the bottom (WM_VSCROLL, SB_BOTTOM), so the app update card is made.
            [void][TssLive]::SendMessageW([TssLive]::GetDlgItem($Win, $UiPanel.Page), 0x115, [IntPtr]7, [IntPtr]::Zero)
            $script:shown = Read-Ui $Win $UiPanel.Page $UiId.AppStatus
            $one = $script:shown -replace '\s+', ' '
            if ($one -match $Pattern) { $Matches[0] }
        } $What 15 200
    } catch { throw "$($_.Exception.Message). Settings shows: $(Ascii $script:shown)" }
}

function Stop-Trays([string]$Root) {
    for ($round = 0; $round -lt 3; $round++) {
        $left = @(Get-Trays $Root)
        if ($left.Count -eq 0) { return }
        foreach ($c in $left) {
            Write-Host "Stopping a leftover tray (pid $($c.ProcessId))." -ForegroundColor Yellow
            try { $p = Get-Tray $c.ProcessId } catch { continue }
            Stop-LiveTray $p
            [void]$p.WaitForExit(5000)
        }
    }
}

$total = [Diagnostics.Stopwatch]::StartNew()
if (-not $NoBuild) {
    Use-Cargo
    Push-Location $RepoRoot
    try { Invoke-Native cargo @('build', '--bins') } finally { Pop-Location }
}

$root2 = $null
$root = New-LiveRoot 'relaunch' 'debug'
try {
    $child = Get-AnonymousAppEnv $root
    $child.TSS_API_BASE = 'http://127.0.0.1:1'
    $child.TSS_RAW_BASE = $child.TSS_API_BASE
    $marks = Join-Path $root 'out\marks.txt'
    $child.TSS_BENCH_REPORT = $marks
    $gui = Join-Path $root 'bin\tiny-skill-sync.exe'
    $skills = Join-Path $root 'skills'
    $expectedTitle = 'Tiny Skill Sync ' + [TssLive]::InstanceTag((Join-Path $root 'home'))

    # A saved setting means this is not a first start, so the tray does not open its window by itself. Automatic
    # app updates off: no scheduled job runs beside the test commands.
    $r = Invoke-LiveTss $root @('update', 'off') $child
    if ($r.Code -ne 0 -or -not (Test-Path -LiteralPath (Join-Path $root 'home\config.json'))) { throw "tss update off failed (exit $($r.Code))." }

    # 1. Window closed: the new tray starts with --restarted and stays in the tray.
    $tray = Start-Tray
    if ((Find-LiveWindow $tray.Id $WINDOW_CLASS -Visible) -ne [IntPtr]::Zero) { throw 'The window opened by itself.' }
    $h = Invoke-Handoff $tray '--restarted'
    $tray = $h.P
    [void](Wait-Live { (Get-MarkCount $marks 'job-startup') -ge 2 } 'the new tray to finish starting' 30 100)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt 1500) {
        if ((Find-LiveWindow $tray.Id $WINDOW_CLASS -Visible) -ne [IntPtr]::Zero) { throw 'The new tray opened its window, which was closed.' }
        Start-Sleep -Milliseconds 100
    }
    Write-Host "PASS: window closed: $($h.Text) and marks file; its window stayed closed."

    # 2. Window open: the new tray starts with --restarted-open, opens it again and says it restarted.
    $win = Open-Window $tray
    $said = Read-Settings $win 'Restarted after the app update\. Now running \d+\.\d+\.\d+\.' 'Settings to say the tray restarted'
    $h = Invoke-Handoff $tray '--restarted-open'
    $tray = $h.P
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $win = Wait-Live { $w = Find-LiveWindow $tray.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 'the new tray to open its window again' 30 100
    $shownMs = $sw.ElapsedMilliseconds
    $sub = Wait-Live { $s = (Read-Hero $win).Sub; if ($s -match 'Restarted after the app update') { $s } } 'the reopened window to say the tray restarted' 10 100
    Write-Host "PASS: window open: $($h.Text); its window showed $shownMs ms later and says '$(Ascii $sub)'. The tray before it showed the throwaway skills folder and said '$(Ascii $said)'"

    # 3. Wrong hash: the tray keeps running, starts nothing and says why on Settings.
    $jobs = Get-MarkCount $marks 'job-app-update'
    $trays = Get-MarkCount $marks 'tray'
    Send-LiveCommand $tray $CMD_TEST_RESTART_WRONG
    [void](Wait-Live { (Get-MarkCount $marks 'job-app-update') -gt $jobs } 'the refused install to finish' 15 100)
    $said = Read-Settings $win 'Could not restart the tray: .+\. Quit and open it again to use the update\.' 'Settings to say the tray could not restart'
    Assert-NoOtherTray $root $tray.Id 5 'Another tray started after a refused restart'
    if ($tray.HasExited) { throw "The tray exited (code $($tray.ExitCode)) after a refused restart." }
    if ((Get-MarkCount $marks 'tray') -ne $trays) { throw 'A tray started after a refused restart.' }
    Write-Host "PASS: wrong hash: pid $($tray.Id) still runs after 5 s and started nothing. Settings (App updates) says '$(Ascii $said)'"

    # 4. A sign-out holds the restart; when Windows calls the sign-out off, the restart goes ahead.
    $jobs = Get-MarkCount $marks 'job-app-update'
    $main = Find-LiveWindow $tray.Id $TRAY_CLASS
    [void][TssLive]::PostMessageW($main, $WM_QUERYENDSESSION, [IntPtr]::Zero, [IntPtr]::Zero)
    Send-LiveCommand $tray $CMD_TEST_RESTART
    [void](Wait-Live { (Get-MarkCount $marks 'job-app-update') -gt $jobs } 'the install to finish' 15 100)
    Assert-NoOtherTray $root $tray.Id 2 'A tray started while Windows was signing out'
    if ($tray.HasExited) { throw "The tray exited (code $($tray.ExitCode)) while Windows was signing out." }
    $h = Invoke-Handoff $tray '--restarted-open' { [void][TssLive]::PostMessageW($main, $WM_ENDSESSION, [IntPtr]::Zero, [IntPtr]::Zero) }
    $tray = $h.P
    [void](Wait-Live { $w = Find-LiveWindow $tray.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 'the new tray to open its window again' 30 100)
    Write-Host "PASS: sign-out: nothing started for 2 s while it was pending; called off: $($h.Text)."

    # 5. Quit stays quit.
    $trays = Get-MarkCount $marks 'tray'
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Send-LiveCommand $tray $CMD_QUIT
    Wait-Exit $tray 'The tray asked to quit' 10
    $quitMs = $sw.ElapsedMilliseconds
    Assert-NoOtherTray $root 0 5 'A tray runs after Quit'
    if ((Get-MarkCount $marks 'tray') -ne $trays) { throw 'A tray started after Quit.' }
    Write-Host "PASS: Quit: pid $($tray.Id) exited 0 after $quitMs ms and nothing ran in the 5 s after."

    # 6. A sign-out that goes ahead while a restart is pending: nothing starts. Posted, so the tray is not ended for
    # real; it is stopped by its process id.
    $tray = Start-Tray
    $jobs = Get-MarkCount $marks 'job-app-update'
    $trays = Get-MarkCount $marks 'tray'
    $main = Find-LiveWindow $tray.Id $TRAY_CLASS
    [void][TssLive]::PostMessageW($main, $WM_QUERYENDSESSION, [IntPtr]::Zero, [IntPtr]::Zero)
    Send-LiveCommand $tray $CMD_TEST_RESTART
    [void](Wait-Live { (Get-MarkCount $marks 'job-app-update') -gt $jobs } 'the install to finish' 15 100)
    [void][TssLive]::PostMessageW($main, $WM_ENDSESSION, [IntPtr]1, [IntPtr]::Zero)
    Assert-NoOtherTray $root $tray.Id 3 'A tray started after Windows signed out'
    if ((Get-MarkCount $marks 'tray') -ne $trays) { throw 'A tray started after Windows signed out.' }
    Stop-Process -Id $tray.Id -Force
    [void]$tray.WaitForExit(5000)
    Write-Host "PASS: sign-out with a restart pending: nothing ran in the 3 s after; pid $($tray.Id) was stopped."

    # 7. A sign-out that begins while the old tray checks the new one: the new one is stopped and the old one stays.
    $tray = Start-Tray
    $trays = Get-MarkCount $marks 'tray'
    $main = Find-LiveWindow $tray.Id $TRAY_CLASS
    Send-LiveCommand $tray $CMD_TEST_RESTART
    $found = @(Wait-Live { @(Get-Trays $root | Where-Object { $_.ProcessId -ne $tray.Id }) } 'a new tray process' 30 20)
    try { $new = Get-Tray $found[0].ProcessId } catch { $new = $null }
    # Sent, as Windows does: the tray takes it when it next looks at its messages, at the end of the check.
    $sw = [Diagnostics.Stopwatch]::StartNew()
    [void][TssLive]::SendMessageW($main, $WM_QUERYENDSESSION, [IntPtr]::Zero, [IntPtr]::Zero)
    $answeredMs = $sw.ElapsedMilliseconds
    if ($new) {
        if (-not $new.WaitForExit(5000)) { throw "The new tray (pid $($new.Id)) still runs after the sign-out began." }
        $new.WaitForExit()
        if ($new.ExitCode -ne 1) { throw "The new tray (pid $($new.Id)) exited with code $($new.ExitCode), not 1 (stopped)." }
    }
    Assert-NoOtherTray $root $tray.Id 2 'A tray started after the sign-out began'
    if ($tray.HasExited) { throw "The tray exited (code $($tray.ExitCode)) when the sign-out began." }
    if ((Get-MarkCount $marks 'tray') -ne $trays) { throw 'A tray started after the sign-out began.' }
    [void][TssLive]::PostMessageW($main, $WM_ENDSESSION, [IntPtr]::Zero, [IntPtr]::Zero)
    $win = Open-Window $tray
    $said = Read-Settings $win 'Could not restart the tray: Windows began to sign out\.' 'Settings to say the sign-out stopped the restart'
    Send-LiveCommand $tray $CMD_QUIT
    Wait-Exit $tray 'The tray asked to quit' 10
    Write-Host "PASS: sign-out during the restart: the tray answered it $answeredMs ms after the new pid $($found[0].ProcessId) showed, stopped that one (exit code 1) and kept running; Settings says '$(Ascii $said)'"

    # 8 and 9. Quit, or a close (as when Windows ends the app), while the old tray checks the new one.
    $said = Invoke-Interrupted (Start-Tray) $WM_COMMAND $CMD_QUIT 'Quit'
    Write-Host "PASS: Quit during the restart: $said."
    $said = Invoke-Interrupted (Start-Tray) $WM_CLOSE 0 'a close'
    Write-Host "PASS: close during the restart: $said."

    # 10. A release build has no test commands.
    $release = Get-ExeDir 'release'
    if (-not (Test-Path -LiteralPath (Join-Path $release 'tiny-skill-sync.exe')) -or -not (Test-Path -LiteralPath (Join-Path $release 'tss.exe'))) {
        Write-Host 'SKIP: no release build'
    } else {
        $root2 = New-LiveRoot 'relaunch'
        $env2 = Get-AnonymousAppEnv $root2
        $env2.TSS_API_BASE = 'http://127.0.0.1:1'
        $env2.TSS_RAW_BASE = $env2.TSS_API_BASE
        $marks2 = Join-Path $root2 'out\marks.txt'
        $env2.TSS_BENCH_REPORT = $marks2
        $rel = Start-LiveTray $root2 @() $env2
        [void](Wait-Live { (Get-MarkCount $marks2 'job-startup') -gt 0 } 'the release tray to finish starting' 30 100)
        Send-LiveCommand $rel $CMD_TEST_RESTART
        Assert-NoOtherTray $root2 $rel.Id 3 'The release tray started another'
        if ($rel.HasExited) { throw "The release tray exited (code $($rel.ExitCode)) on the test command." }
        if ((Get-MarkCount $marks2 'tray') -ne 1 -or (Get-MarkCount $marks2 'job-app-update') -ne 0) { throw 'The release tray acted on the test command.' }
        Send-LiveCommand $rel $CMD_QUIT
        Wait-Exit $rel 'The release tray' 10
        $built = (Get-Item -LiteralPath (Join-Path $release 'tiny-skill-sync.exe')).LastWriteTime.ToString('yyyy-MM-dd HH:mm')
        Write-Host "PASS: release build ($built): pid $($rel.Id) ignored the test command for 3 s, then quit with 0."
    }
    Write-Host ("All checks passed in {0:N1} s." -f $total.Elapsed.TotalSeconds)
} finally {
    $kept = @()
    foreach ($r in @($root, $root2)) {
        if (-not $r) { continue }
        try { Stop-Trays $r } finally { Remove-LiveRoot $r }
        if (Test-Path -LiteralPath $r) { $kept += $r }
    }
    if ($kept.Count) { throw "Could not remove this run's folder: $($kept -join ', ')" }
}
