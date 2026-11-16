<#
.SYNOPSIS
  Measures the release tray app against its budgets: startup, idle cost, memory, the settings window and an
  unchanged poll. Prints a table and saves it in bench\out\.

.DESCRIPTION
  Everything happens in a throwaway folder in %TEMP% (deleted at the end unless -Keep). Real skills are downloaded
  into it from -Repo, so this needs the network.

  1. Install: 'tss add' and 'tss check' fill the throwaway skills folder.
  2. Startup: the tray app starts -Runs times on that install (warm: the files are in the cache). The app writes
     timing marks to a file named by TSS_BENCH_REPORT (see src\win\ui\bench.rs).
  3. Idle: one copy is watched for -IdleSeconds: CPU, I/O and page faults should stay at 0.
  4. Settings: a second start makes the first copy open its settings window, which shows on the screen for about
     2 seconds and is then closed; memory is measured on it. The time to open is measured -SettingsRuns times, each
     time the first opening in a fresh copy (the window flashes up each time). Skip with -NoSettings.
  5. Poll: the record is changed so every repository is due, and the app is started again: it checks GitHub at
     once. Nothing changed upstream, so it should get one "not modified" answer and touch no files.

  Counters are the ones Task Manager uses (WMI Win32_PerfRawData_PerfProc_Process). File reads and writes are
  counted apart from network traffic ("other" I/O).

.PARAMETER Repo
  Repositories to install, as for 'tss add'. Default: the skills folder of anthropics/skills.

.PARAMETER UseToken
  Send the GitHub token (needed for private repositories).

.PARAMETER Runs
  How many times to measure startup. Default 10.

.PARAMETER IdleSeconds
  How long to watch the idle app. Default 10.

.PARAMETER NoSettings
  Do not open the settings window.

.PARAMETER SettingsRuns
  How many times to measure opening the settings window. Default 5.

.PARAMETER Keep
  Keep the throwaway folder, to look at it afterwards.

.EXAMPLE
  scripts\bench.ps1
  scripts\bench.ps1 -Repo https://github.com/github/awesome-copilot/tree/main/skills -Runs 5
#>
param(
    [string[]]$Repo = @('https://github.com/anthropics/skills/tree/main/skills'),
    [switch]$UseToken,
    [int]$Runs = 10,
    [int]$IdleSeconds = 10,
    [switch]$NoSettings,
    [int]$SettingsRuns = 5,
    [switch]$Keep
)

. "$PSScriptRoot\common.ps1"

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class TssBenchNative {
    [DllImport("kernel32.dll")] public static extern bool QueryProcessCycleTime(IntPtr process, out ulong cycles);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string cls, string title);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
}
'@

$WM_COMMAND = 0x111
$WM_CLOSE = 0x10
$CMD_QUIT = 1003
$TRAY_CLASS = 'TinySkillSyncTray'
$SETTINGS_CLASS = 'TinySkillSyncSettings'

$exe = Join-Path (Get-ExeDir 'release') 'tiny-skill-sync.exe'
$tssExe = Join-Path (Get-ExeDir 'release') 'tss.exe'
$root = Join-Path $env:TEMP ('tss-bench-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$homeDir = Join-Path $root 'home'
$skillsDir = Join-Path $root 'skills'
$reports = Join-Path $root 'reports'
$started = New-Object System.Collections.Generic.List[object]
$rows = New-Object System.Collections.Generic.List[object]
$notes = New-Object System.Collections.Generic.List[string]

# ---------------------------------------------------------------- helpers

function Get-Stats([double[]]$Values) {
    $s = @($Values | Sort-Object)
    $n = $s.Count
    $median = if ($n % 2 -eq 1) { $s[($n - 1) / 2] } else { ($s[$n / 2 - 1] + $s[$n / 2]) / 2 }
    $p95 = $s[[math]::Min($n - 1, [int][math]::Ceiling(0.95 * $n) - 1)]
    [pscustomobject]@{ N = $n; Min = $s[0]; Median = $median; P95 = $p95; Max = $s[$n - 1] }
}

function Add-Row([string]$Measure, [string]$Result, [string]$Budget, $Ok) {
    $verdict = if ($null -eq $Ok) { '' } elseif ($Ok) { 'yes' } else { 'NO' }
    $rows.Add([pscustomobject]@{ Measure = $Measure; Result = $Result; Budget = $Budget; Within = $verdict })
}

function Start-Tray([string]$Report, [string[]]$AppArgs = @()) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = $AppArgs -join ' '
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['TSS_HOME'] = $homeDir
    $psi.EnvironmentVariables['TSS_SKILLS_DIR'] = $skillsDir
    $psi.EnvironmentVariables['TSS_BENCH_REPORT'] = $Report
    $p = [Diagnostics.Process]::Start($psi)
    $started.Add($p)
    $p
}

# Every mark in a report, in order. The app may be writing to it, so the file is opened to share writing.
function Read-Marks([string]$Report) {
    $text = ''
    try {
        $fs = New-Object IO.FileStream($Report, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
        try { $text = (New-Object IO.StreamReader($fs)).ReadToEnd() } finally { $fs.Dispose() }
    } catch { return @() }
    # Only whole lines: the last piece is empty, or a line the app is still writing.
    $lines = $text -split "`r`n"
    if ($lines.Count -lt 2) { return }
    foreach ($line in $lines[0..($lines.Count - 2)]) {
        if (-not $line) { continue }
        $m = @{}
        foreach ($pair in $line -split ' ') { $kv = $pair -split '=', 2; if ($kv.Count -eq 2) { $m[$kv[0]] = $kv[1] } }
        [pscustomobject]@{
            Mark   = $m['mark']
            AtMs   = [double]$m['at_ms']
            CpuMs  = [double]$m['cpu_ms']
            TookMs = if ($m.ContainsKey('took_ms')) { [double]$m['took_ms'] } else { $null }
        }
    }
}

# The I/O the marks themselves cost so far: the app writes each line with one call.
function Get-MarkIo([string]$Report) {
    $marks = @(Read-Marks $Report)
    [pscustomobject]@{ Ops = $marks.Count; Bytes = (Get-Item $Report).Length }
}

function Wait-Mark([string]$Report, [string]$Mark, [int]$TimeoutMs = 20000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $hit = @(Read-Marks $Report | Where-Object { $_.Mark -eq $Mark }) | Select-Object -First 1
        if ($hit) { return $hit }
        Start-Sleep -Milliseconds 5
    }
    throw "The app did not report '$Mark' within $($TimeoutMs / 1000) s (report: $Report)."
}

function Find-Window([string]$Class, [int]$ProcId) {
    $h = [IntPtr]::Zero
    while ($true) {
        # PowerShell 5.1 turns $null into "" for a string parameter, which would only match an empty title.
        $h = [TssBenchNative]::FindWindowExW([IntPtr]::Zero, $h, $Class, [NullString]::Value)
        if ($h -eq [IntPtr]::Zero) { return $h }
        [uint32]$owner = 0
        [void][TssBenchNative]::GetWindowThreadProcessId($h, [ref]$owner)
        if ($owner -eq $ProcId) { return $h }
    }
}

function Wait-Window([string]$Class, [int]$ProcId, [int]$TimeoutMs = 5000, [switch]$Gone) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $h = Find-Window $Class $ProcId
        if ($Gone -and $h -eq [IntPtr]::Zero) { return $true }
        if (-not $Gone -and $h -ne [IntPtr]::Zero) { return $h }
        Start-Sleep -Milliseconds 5
    }
    if ($Gone) { $false } else { [IntPtr]::Zero }
}

function Stop-Tray([Diagnostics.Process]$P) {
    if ($P.HasExited) { return }
    $h = Find-Window $TRAY_CLASS $P.Id
    if ($h -ne [IntPtr]::Zero) { [void][TssBenchNative]::PostMessageW($h, $WM_COMMAND, [IntPtr]$CMD_QUIT, [IntPtr]::Zero) }
    if (-not $P.WaitForExit(5000)) {
        Stop-Process -Id $P.Id -Force
        throw "The app did not quit within 5 s; it was stopped."
    }
}

function Get-Snapshot([Diagnostics.Process]$P) {
    $P.Refresh()
    [uint64]$cycles = 0
    [void][TssBenchNative]::QueryProcessCycleTime($P.Handle, [ref]$cycles)
    $q = Get-CimInstance -ClassName Win32_PerfRawData_PerfProc_Process -Filter ("IDProcess={0}" -f $P.Id)
    [pscustomobject]@{
        WorkingSetKiB = [int64]($P.WorkingSet64 / 1KB)
        PrivateKiB    = [int64]($P.PrivateMemorySize64 / 1KB)
        Threads       = [int]$q.ThreadCount
        Handles       = [int]$q.HandleCount
        Cycles        = [int64]$cycles
        CpuMs         = [double]([uint64]$q.PercentProcessorTime) / 10000.0
        ReadOps       = [int64]$q.IOReadOperationsPersec
        WriteOps      = [int64]$q.IOWriteOperationsPersec
        OtherOps      = [int64]$q.IOOtherOperationsPersec
        ReadBytes     = [int64]$q.IOReadBytesPersec
        WriteBytes    = [int64]$q.IOWriteBytesPersec
        PageFaults    = [int64]$q.PageFaultsPersec
    }
}

function Get-Delta($A, $B) {
    $d = [ordered]@{}
    foreach ($name in 'Cycles', 'CpuMs', 'ReadOps', 'WriteOps', 'OtherOps', 'ReadBytes', 'WriteBytes', 'PageFaults') {
        $d[$name] = $B.$name - $A.$name
    }
    [pscustomobject]$d
}

# Size and time of every file under the settings and skills folders, to see what a step wrote.
function Get-Files {
    $map = @{}
    foreach ($dir in $homeDir, $skillsDir) {
        Get-ChildItem $dir -Recurse -File -Force -ErrorAction SilentlyContinue | ForEach-Object {
            $map[$_.FullName.Substring($root.Length + 1)] = '{0}|{1}' -f $_.Length, $_.LastWriteTimeUtc.Ticks
        }
    }
    $map
}

function Compare-Files($Before, $After) {
    $changed = @()
    foreach ($k in $After.Keys) { if (-not $Before.ContainsKey($k)) { $changed += "$k (new)" } elseif ($Before[$k] -ne $After[$k]) { $changed += $k } }
    foreach ($k in $Before.Keys) { if (-not $After.ContainsKey($k)) { $changed += "$k (deleted)" } }
    $sorted = @($changed | Sort-Object)
    , $sorted
}

function Format-List2($Items) {
    if (@($Items).Count -eq 0) { 'none' } else { ($Items -join ', ') }
}

function MiB([double]$KiB) { '{0:N1} MiB' -f ($KiB / 1024) }

# ---------------------------------------------------------------- the run

& "$PSScriptRoot\build.ps1" | Out-Host
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
New-Item -ItemType Directory -Force $reports | Out-Null

try {
    # 1. Install
    Write-Host "== Install into $root" -ForegroundColor Cyan
    $install = Invoke-InSandbox -Root $root -Body {
        $ErrorActionPreference = 'Continue'
        foreach ($r in $Repo) {
            $a = @('add', $r)
            if ($UseToken) { $a += '--use-token' }
            & $tssExe @a | Out-Host
            if ($LASTEXITCODE -ne 0) { throw "tss add $r failed with exit code $LASTEXITCODE" }
        }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        & $tssExe check | Out-Host
        [pscustomobject]@{ Code = $LASTEXITCODE; Ms = $sw.Elapsed.TotalMilliseconds }
    }
    if ($install.Code -eq 2) { throw 'tss check failed: see above.' }
    $files = @(Get-ChildItem $skillsDir -Recurse -File -Force)
    $fileCount = $files.Count
    $bytes = ($files | Measure-Object Length -Sum).Sum
    $skillCount = @(Get-ChildItem $skillsDir -Directory).Count
    $notes.Add(('Install: {0} skills, {1} files, {2:N1} MiB from {3}; tss check took {4:N0} ms (exit code {5}).' -f $skillCount, $fileCount, ($bytes / 1MB), ($Repo -join ', '), $install.Ms, $install.Code))

    # 2. Startup. The first start of a new build is not counted: Windows Defender scans it once (0.5 to 1 s).
    Write-Host "== Startup x $Runs" -ForegroundColor Cyan
    $filesBefore = Get-Files
    $warm = Join-Path $reports 'warm-up.txt'
    $p = Start-Tray $warm
    [void](Wait-Mark $warm 'job-startup' 60000)
    Stop-Tray $p
    $tray = @(); $verified = @(); $verifyTook = @(); $startCpu = @(); $quitMs = @()
    for ($i = 1; $i -le $Runs; $i++) {
        $report = Join-Path $reports "startup-$i.txt"
        $p = Start-Tray $report
        $t = Wait-Mark $report 'tray'
        $v = Wait-Mark $report 'job-startup'
        Stop-Tray $p
        $q = Wait-Mark $report 'quit' 1000
        $x = Wait-Mark $report 'exit' 1000
        $tray += $t.AtMs; $verified += $v.AtMs; $verifyTook += $v.TookMs; $startCpu += $v.CpuMs; $quitMs += ($x.AtMs - $q.AtMs)
    }
    $writtenByStartup = Compare-Files $filesBefore (Get-Files)
    $s = Get-Stats $tray
    Add-Row 'Process creation to tray icon added' ('median {0:N1} ms, p95 {1:N1} ms (min {2:N1}, max {3:N1}, n {4})' -f $s.Median, $s.P95, $s.Min, $s.Max, $s.N) 'median 30 ms or less, p95 60 ms or less' ($s.Median -le 30 -and $s.P95 -le 60)
    $v = Get-Stats $verified
    Add-Row 'Process creation to install verified' ('median {0:N1} ms, p95 {1:N1} ms' -f $v.Median, $v.P95) '' $null
    $vt = Get-Stats $verifyTook
    $perFileUs = $vt.Median * 1000 / [math]::Max(1, $fileCount)
    if ($fileCount -le 84) { $verifyOk = $vt.Median -le 10; $verifyBudget = '10 ms or less for 84 files' }
    else { $verifyOk = $perFileUs -le 75; $verifyBudget = '75 microseconds or less per file' }
    Add-Row "Startup job: load the record, verify $fileCount files" ('median {0:N2} ms, p95 {1:N2} ms ({2:N1} microseconds per file)' -f $vt.Median, $vt.P95, $perFileUs) $verifyBudget $verifyOk
    $c = Get-Stats $startCpu
    Add-Row 'CPU used by startup, up to verified' ('median {0:N1} ms' -f $c.Median) '' $null
    $qs = Get-Stats $quitMs
    Add-Row 'Quit to exit' ('median {0:N1} ms, max {1:N1} ms' -f $qs.Median, $qs.Max) '2 s or less (quit wait)' ($qs.Max -le 2000)
    Add-Row 'Files written by startup and quit (from the first start after tss check)' (Format-List2 $writtenByStartup) 'none' (@($writtenByStartup).Count -eq 0)

    # 3. Idle
    Write-Host "== Idle $IdleSeconds s" -ForegroundColor Cyan
    $report = Join-Path $reports 'idle.txt'
    $p = Start-Tray $report
    [void](Wait-Mark $report 'job-startup')
    Start-Sleep -Seconds 2
    $a = Get-Snapshot $p
    $idleMarks = Get-MarkIo $report
    Start-Sleep -Seconds $IdleSeconds
    $b = Get-Snapshot $p
    $d = Get-Delta $a $b
    $afterStartup = $a
    $idleOk = $d.CpuMs -eq 0 -and $d.ReadOps -eq 0 -and $d.WriteOps -eq 0 -and $d.OtherOps -eq 0 -and $d.PageFaults -eq 0
    Add-Row "Idle for $IdleSeconds s" ('CPU {0} ms ({1:N0} cycles), I/O operations read/write/other {2}/{3}/{4}, page faults {5}' -f $d.CpuMs, $d.Cycles, $d.ReadOps, $d.WriteOps, $d.OtherOps, $d.PageFaults) '0 CPU, 0 I/O, 0 page faults' $idleOk
    Add-Row 'Idle working set (trimmed after startup) / private bytes' ('{0} / {1}' -f (MiB $b.WorkingSetKiB), (MiB $b.PrivateKiB)) '2 MiB or less trimmed / 4 MiB or less' ($b.WorkingSetKiB -le 2048 -and $b.PrivateKiB -le 4096)
    Add-Row 'Idle threads / handles' ('{0} / {1}' -f $b.Threads, $b.Handles) '' $null

    # 4. Settings
    if (-not $NoSettings) {
        Write-Host "== Settings window (shows for about 2 s, then flashes up $($SettingsRuns - 1) more times)" -ForegroundColor Cyan
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $second = Start-Tray (Join-Path $reports 'second.txt')
        if (-not $second.WaitForExit(5000)) { throw 'The second copy did not hand over to the first within 5 s.' }
        $st = Wait-Mark $report 'settings' 5000
        $wallMs = $sw.Elapsed.TotalMilliseconds
        $openMs = @($st.TookMs)
        Add-Row 'Second start: exit code' ([string]$second.ExitCode) '0, and the first copy opens its settings' ($second.ExitCode -eq 0)
        Start-Sleep -Seconds 1
        $open = Get-Snapshot $p
        Add-Row 'Settings window open: working set / private bytes' ('{0} / {1}' -f (MiB $open.WorkingSetKiB), (MiB $open.PrivateKiB)) '20 MiB or less' ($open.WorkingSetKiB -le 20480)
        $h = Find-Window $SETTINGS_CLASS $p.Id
        if ($h -eq [IntPtr]::Zero) { throw 'The settings window was not found.' }
        [void][TssBenchNative]::PostMessageW($h, $WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero)
        if (-not (Wait-Window $SETTINGS_CLASS $p.Id 5000 -Gone)) { throw 'The settings window did not close.' }
        Start-Sleep -Seconds 1
        $closed = Get-Snapshot $p
        Add-Row 'Settings window closed: working set / private bytes' ('{0} / {1}' -f (MiB $closed.WorkingSetKiB), (MiB $closed.PrivateKiB)) '2 MiB or less trimmed' ($closed.WorkingSetKiB -le 2048)
        # Only one copy runs at a time, so the idle one goes before the fresh ones start.
        Stop-Tray $p
        for ($k = 2; $k -le $SettingsRuns; $k++) {
            $r = Join-Path $reports "settings-$k.txt"
            $q = Start-Tray $r
            [void](Wait-Mark $r 'job-startup')
            $again = Start-Tray (Join-Path $reports "second-$k.txt")
            if (-not $again.WaitForExit(5000)) { throw 'The second copy did not hand over to the first within 5 s.' }
            $openMs += (Wait-Mark $r 'settings' 5000).TookMs
            $h = Wait-Window $SETTINGS_CLASS $q.Id
            if ($h -eq [IntPtr]::Zero) { throw 'The settings window was not found.' }
            [void][TssBenchNative]::PostMessageW($h, $WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero)
            if (-not (Wait-Window $SETTINGS_CLASS $q.Id 5000 -Gone)) { throw 'The settings window did not close.' }
            Stop-Tray $q
        }
        $so = Get-Stats $openMs
        Add-Row 'Settings window: open to painted (first opening in a copy)' ('median {0:N1} ms, max {1:N1} ms (n {2}); second start to painted {3:N0} ms wall' -f $so.Median, $so.Max, $so.N, $wallMs) 'median 50 ms or less' ($so.Median -le 50)
    }
    Stop-Tray $p

    # 5. Poll
    Write-Host '== Unchanged poll' -ForegroundColor Cyan
    # Make every repository due. The record is edited as text: a JSON round trip in PowerShell could round large numbers.
    $stateFile = Join-Path $homeDir 'state.json'
    $due = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds() - 2 * 86400
    $text = [IO.File]::ReadAllText($stateFile)
    $edited = [regex]::Replace($text, '("last_check"\s*:\s*)\d+', ('${{1}}{0}' -f $due))
    if ($edited -eq $text) { throw "No last_check was found in $stateFile." }
    [IO.File]::WriteAllText($stateFile, $edited)
    $filesBefore = Get-Files
    $report = Join-Path $reports 'poll.txt'
    $p = Start-Tray $report
    $start = Wait-Mark $report 'job-startup'
    $poll = Wait-Mark $report 'job-poll' 60000
    Start-Sleep -Seconds 8
    # Marks are counted before the snapshot, so a mark written in between is never subtracted without being counted.
    $pollMarks = Get-MarkIo $report
    $after = Get-Snapshot $p
    $writtenByPoll = Compare-Files $filesBefore (Get-Files)
    $io = Get-Delta $afterStartup $after
    # The poll run wrote more marks than the idle run had at its snapshot; those writes are the bench's, not the app's.
    $extraMarks = $pollMarks.Ops - $idleMarks.Ops
    $io.WriteOps -= $extraMarks
    $io.WriteBytes -= $pollMarks.Bytes - $idleMarks.Bytes
    $pollCpu = $poll.CpuMs - $start.CpuMs
    Add-Row 'Unchanged poll: time / CPU' ('{0:N0} ms / {1:N1} ms CPU (process timer ticks are 15.6 ms)' -f $poll.TookMs, $pollCpu) '50 ms CPU or less' ($pollCpu -le 50)
    Add-Row 'Unchanged poll: file I/O' ('read/write operations {0}/{1} ({2:N0}/{3:N0} bytes), other {4}; {5} extra bench mark line(s) left out' -f $io.ReadOps, $io.WriteOps, $io.ReadBytes, $io.WriteBytes, $io.OtherOps, $extraMarks) '0 file reads or writes' ($io.ReadOps -eq 0 -and $io.WriteOps -eq 0)
    Add-Row 'Unchanged poll: files changed' (Format-List2 $writtenByPoll) 'none' (@($writtenByPoll).Count -eq 0)
    Add-Row '8 s after a poll: working set / private bytes' ('{0} / {1}' -f (MiB $after.WorkingSetKiB), (MiB $after.PrivateKiB)) '20 MiB or less / 5 MiB or less' ($after.WorkingSetKiB -le 20480 -and $after.PrivateKiB -le 5120)
    Add-Row '8 s after a poll: threads / handles' ('{0} / {1}' -f $after.Threads, $after.Handles) '10 / 350 or less' ($after.Threads -le 10 -and $after.Handles -le 350)
    $filesBefore = Get-Files
    Stop-Tray $p
    Add-Row 'Files written when quitting after the poll' (Format-List2 (Compare-Files $filesBefore (Get-Files))) '' $null
    $notes.Add('Poll I/O is the poll run (startup, poll, 8 s) minus the idle run after startup (both start the same way), minus the bench''s own extra mark lines.')

    # 6. Size
    $size = (Get-Item $exe).Length
    Add-Row 'tiny-skill-sync.exe' ('{0:N0} bytes ({1:N2} MiB)' -f $size, ($size / 1MB)) '1 MiB or less' ($size -le 1MB)
} finally {
    foreach ($p in $started) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
    if ($Keep) { Write-Host "Kept: $root" } elseif (Test-Path $root) { Start-Sleep -Milliseconds 200; Remove-Item -Recurse -Force $root }
}

# ---------------------------------------------------------------- the result

$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
$os = (Get-CimInstance Win32_OperatingSystem).Caption
$lines = @(
    "# Bench $stamp",
    '',
    "Host: $env:COMPUTERNAME, $cpu, $os. Warm runs (files in the cache).",
    '',
    '| Measure | Result | Budget | Within |',
    '| --- | --- | --- | --- |'
)
foreach ($r in $rows) { $lines += '| {0} | {1} | {2} | {3} |' -f $r.Measure, $r.Result, $r.Budget, $r.Within }
$lines += ''
foreach ($n in $notes) { $lines += "- $n" }
$outDir = Join-Path $RepoRoot 'bench\out'
New-Item -ItemType Directory -Force $outDir | Out-Null
$outFile = Join-Path $outDir "bench-$stamp.md"
[IO.File]::WriteAllLines($outFile, [string[]]$lines)
$lines | Out-Host
Write-Host "Saved: $outFile"
if (@($rows | Where-Object { $_.Within -eq 'NO' }).Count -gt 0) { exit 1 }
