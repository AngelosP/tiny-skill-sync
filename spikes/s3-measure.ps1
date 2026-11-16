<#
.SYNOPSIS
  Spike S3: measures start-up, idle cost, settings-window cost and poll cost of the tray examples.

.DESCRIPTION
  startup   N runs of `s3_tray --exit-after-ready` interleaved with N runs of the empty `s3_floor`, per variant.
            Wall clock is measured around Process.Start/WaitForExit; phases come from inside the process
            (GetProcessTimes creation time -> GetSystemTimePreciseAsFileTime marks).
  idle      starts the tray, lets it settle, then compares counters before/after $IdleSeconds of doing nothing.
  settings  opens and closes the settings window 10 times and watches memory, handles and threads for leaks.
  poll      one worker-thread poll (two WinHTTP requests); counters before, after, and after a quiet period.
  modules   which DLLs are loaded at idle, after opening settings, and after a poll.
  threads   thread, handle and module counts at idle, after a poll, and $SettleSeconds after the poll.
  coldstart first launch of a never-seen exe (an install or update) against its second launch. Each run writes a
            unique copy (the original bytes plus 32 random overlay bytes) so Defender has no cached verdict for it.
            -ColdDelaySeconds waits between writing the copy and launching it.

  Counters: Get-Process memory, QueryProcessCycleTime, and the WMI raw performance classes
  Win32_PerfRawData_PerfProc_Process / _Thread (cumulative I/O operations and bytes, page faults,
  context switches). They work for a standard user. A real file-operation trace (ETW kernel file provider)
  needs elevation or the Performance Log Users group, so it is NOT available here.

  The GitHub token for -Auth token comes from `gh auth token` and is only placed in the child's environment.
  It is never printed or stored.
#>
[CmdletBinding()]
param(
    [ValidateSet('startup', 'idle', 'settings', 'poll', 'modules', 'threads', 'coldstart', 'all')]
    [string]$Scenario = 'all',
    [string]$Dir = 'target-crt\release\examples',
    [int]$Runs = 30,
    [int]$IdleSeconds = 60,
    [int]$SettleSeconds = 62,
    [int]$ColdDelaySeconds = 0,
    [switch]$Trim,
    [ValidateSet('anonymous', 'token')]
    [string]$Auth = 'token',
    [string]$Label = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$work = Join-Path $env:TEMP 'tss-s3'
$outDir = Join-Path $PSScriptRoot 'out'
New-Item -ItemType Directory -Force $work | Out-Null
New-Item -ItemType Directory -Force $outDir | Out-Null
$resultsFile = Join-Path $outDir 's3-results.jsonl'
$started = New-Object System.Collections.Generic.List[int]
$dirWasSet = $PSBoundParameters.ContainsKey('Dir')

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class TssNative {
    [DllImport("kernel32.dll")] public static extern bool QueryProcessCycleTime(IntPtr process, out ulong cycles);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
}
'@

$WM_COMMAND = 0x111
$WM_CLOSE = 0x10
$CMD_SETTINGS = 1001
$CMD_QUIT = 1002
$CMD_POLL = 1003

function Get-Stats([double[]]$Values) {
    $s = @($Values | Sort-Object)
    $n = $s.Count
    $median = if ($n % 2 -eq 1) { $s[($n - 1) / 2] } else { ($s[$n / 2 - 1] + $s[$n / 2]) / 2 }
    $p95 = $s[[math]::Min($n - 1, [int][math]::Ceiling(0.95 * $n) - 1)]
    [pscustomobject]@{ n = $n; min = $s[0]; median = $median; p95 = $p95; max = $s[$n - 1] }
}

function Save-Result($Object) {
    ($Object | ConvertTo-Json -Compress -Depth 6) | Add-Content -Path $resultsFile -Encoding UTF8
}

function Read-Report([string]$Path) {
    $map = @{}
    if (Test-Path $Path) {
        foreach ($line in Get-Content $Path) {
            $i = $line.IndexOf('=')
            if ($i -gt 0) { $map[$line.Substring(0, $i)] = $line.Substring($i + 1) }
        }
    }
    $map
}

function Start-App([string]$Exe, [string[]]$AppArgs, [hashtable]$ChildEnv) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = ($AppArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $psi.UseShellExecute = $false
    if ($ChildEnv) { foreach ($k in $ChildEnv.Keys) { $psi.EnvironmentVariables[$k] = $ChildEnv[$k] } }
    $p = [Diagnostics.Process]::Start($psi)
    $started.Add($p.Id)
    $p
}

function Find-OwnWindow([string]$Class, [int]$ProcId) {
    # PowerShell 5.1 turns a plain $null into "" for a string P/Invoke parameter, which only matches windows with an empty title.
    $h = [TssNative]::FindWindowW($Class, [NullString]::Value)
    if ($h -eq [IntPtr]::Zero) { return [IntPtr]::Zero }
    [uint32]$owner = 0
    [void][TssNative]::GetWindowThreadProcessId($h, [ref]$owner)
    if ($owner -eq $ProcId) { $h } else { [IntPtr]::Zero }
}

function Wait-Window([string]$Class, [int]$ProcId, [int]$TimeoutMs = 5000, [switch]$Gone) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $h = Find-OwnWindow $Class $ProcId
        if ($Gone) { if ($h -eq [IntPtr]::Zero) { return $sw.Elapsed.TotalMilliseconds } }
        elseif ($h -ne [IntPtr]::Zero) { return [pscustomobject]@{ Handle = $h; Ms = $sw.Elapsed.TotalMilliseconds } }
        [Threading.Thread]::Sleep(1)
    }
    $null
}

function Send-Command([IntPtr]$Hwnd, [int]$Id) {
    [void][TssNative]::PostMessageW($Hwnd, $WM_COMMAND, [IntPtr]$Id, [IntPtr]::Zero)
}

function Stop-App($P, [IntPtr]$Hwnd) {
    if (-not $P.HasExited) {
        Send-Command $Hwnd $CMD_QUIT
        if (-not $P.WaitForExit(5000)) { Stop-Process -Id $P.Id -Force }
    }
}

function Get-Snapshot([Diagnostics.Process]$P) {
    $P.Refresh()
    [uint64]$cycles = 0
    [void][TssNative]::QueryProcessCycleTime($P.Handle, [ref]$cycles)
    $q = Get-CimInstance -ClassName Win32_PerfRawData_PerfProc_Process -Filter ("IDProcess={0}" -f $P.Id)
    $threadCtx = @{}
    foreach ($t in @(Get-CimInstance -ClassName Win32_PerfRawData_PerfProc_Thread -Filter ("IDProcess={0}" -f $P.Id))) {
        $threadCtx[[string]$t.IDThread] = [int64]$t.ContextSwitchesPersec
    }
    [pscustomobject]@{
        WorkingSetKiB  = [int64]($P.WorkingSet64 / 1KB)
        PrivateKiB     = [int64]($P.PrivateMemorySize64 / 1KB)
        PrivateWsKiB   = [int64]([uint64]$q.WorkingSetPrivate / 1KB)
        Threads        = [int]$q.ThreadCount
        Handles        = [int]$q.HandleCount
        Modules        = $P.Modules.Count
        Cycles         = [int64]$cycles
        CpuMs          = [double]([uint64]$q.PercentProcessorTime) / 10000.0
        ThreadCtx      = $threadCtx
        IoReadOps      = [int64]$q.IOReadOperationsPersec
        IoWriteOps     = [int64]$q.IOWriteOperationsPersec
        IoOtherOps     = [int64]$q.IOOtherOperationsPersec
        IoReadBytes    = [int64]$q.IOReadBytesPersec
        IoWriteBytes   = [int64]$q.IOWriteBytesPersec
        IoOtherBytes   = [int64]$q.IOOtherBytesPersec
        PageFaults     = [int64]$q.PageFaultsPersec
    }
}

function Get-Delta($A, $B) {
    # Context switches are summed only over threads present in both snapshots; a thread that exits would otherwise make the sum go down.
    $ctx = [int64]0
    $appeared = 0
    foreach ($id in $B.ThreadCtx.Keys) {
        if ($A.ThreadCtx.ContainsKey($id)) { $ctx += $B.ThreadCtx[$id] - $A.ThreadCtx[$id] } else { $appeared++ }
    }
    $gone = @($A.ThreadCtx.Keys | Where-Object { -not $B.ThreadCtx.ContainsKey($_) }).Count
    [pscustomobject]@{
        Cycles          = $B.Cycles - $A.Cycles
        CpuMs           = [math]::Round($B.CpuMs - $A.CpuMs, 2)
        CtxSwitches     = $ctx
        ThreadsAppeared = $appeared
        ThreadsGone     = $gone
        IoReadOps       = $B.IoReadOps - $A.IoReadOps
        IoWriteOps      = $B.IoWriteOps - $A.IoWriteOps
        IoOtherOps      = $B.IoOtherOps - $A.IoOtherOps
        IoReadBytes     = $B.IoReadBytes - $A.IoReadBytes
        IoWriteBytes    = $B.IoWriteBytes - $A.IoWriteBytes
        IoOtherBytes    = $B.IoOtherBytes - $A.IoOtherBytes
        PageFaults      = $B.PageFaults - $A.PageFaults
    }
}

function Get-AppArgs([string]$Report, [switch]$ExitAfterReady) {
    $a = @('--report', $Report)
    if ($ExitAfterReady) { $a += '--exit-after-ready' }
    if ($Trim) { $a += '--trim' }
    $a
}

function Get-Token {
    if ($Auth -ne 'token') { return $null }
    $t = (& gh auth token 2>$null | Select-Object -First 1)
    if (-not $t) { throw 'gh auth token returned nothing; run `gh auth login` or use -Auth anonymous.' }
    $t.Trim()
}

function Invoke-Startup {
    $variants = @(
        @{ Name = 'dyn-release'; Dir = 'target\release\examples' },
        @{ Name = 'dyn-size-s'; Dir = 'target\size-s\examples' },
        @{ Name = 'dyn-size-z'; Dir = 'target\size-z\examples' },
        @{ Name = 'crt-release'; Dir = 'target-crt\release\examples' },
        @{ Name = 'crt-size-s'; Dir = 'target-crt\size-s\examples' },
        @{ Name = 'crt-size-z'; Dir = 'target-crt\size-z\examples' }
    )
    if ($dirWasSet) { $variants = @(@{ Name = $Dir; Dir = $Dir }) }
    foreach ($v in $variants) {
        $tray = Join-Path $root "$($v.Dir)\s3_tray.exe"
        $floor = Join-Path $root "$($v.Dir)\s3_floor.exe"
        if (-not (Test-Path $tray)) { "skip $($v.Name): not built"; continue }
        $wallTray = New-Object System.Collections.Generic.List[double]
        $wallFloor = New-Object System.Collections.Generic.List[double]
        $cols = @{}
        foreach ($k in 'creation_to_main', 'main_to_window', 'window_to_icon', 'icon_to_tray', 'creation_to_tray', 'ready_ws', 'ready_private', 'ready_cpu', 'ready_cycles') {
            $cols[$k] = New-Object System.Collections.Generic.List[double]
        }
        $notAdded = 0
        for ($i = 1; $i -le $Runs; $i++) {
            $report = Join-Path $work "startup-$i.txt"
            Remove-Item $report -ErrorAction SilentlyContinue
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $p = Start-App $floor @() $null
            [void]$p.WaitForExit(10000)
            $sw.Stop()
            $wallFloor.Add($sw.Elapsed.TotalMilliseconds)

            $sw = [Diagnostics.Stopwatch]::StartNew()
            $p = Start-App $tray (Get-AppArgs $report -ExitAfterReady) $null
            [void]$p.WaitForExit(10000)
            $sw.Stop()
            $wallTray.Add($sw.Elapsed.TotalMilliseconds)
            $r = Read-Report $report
            if ($r['tray.added'] -ne 'true') { $notAdded++ }
            $m = { param($k) [double]::Parse($r[$k], [Globalization.CultureInfo]::InvariantCulture) }
            $cols['creation_to_main'].Add((& $m 'mark.main_ms'))
            $cols['main_to_window'].Add((& $m 'mark.window_created_ms') - (& $m 'mark.main_ms'))
            $cols['window_to_icon'].Add((& $m 'mark.icon_built_ms') - (& $m 'mark.window_created_ms'))
            $cols['icon_to_tray'].Add((& $m 'mark.tray_added_ms') - (& $m 'mark.icon_built_ms'))
            $cols['creation_to_tray'].Add((& $m 'mark.tray_added_ms'))
            $cols['ready_ws'].Add((& $m 'ready.working_set_kib'))
            $cols['ready_private'].Add((& $m 'ready.private_kib'))
            $cols['ready_cpu'].Add((& $m 'ready.cpu_user_ms') + (& $m 'ready.cpu_kernel_ms'))
            $cols['ready_cycles'].Add((& $m 'ready.cycles'))
        }
        $row = [ordered]@{
            variant = $v.Name; runs = $Runs; tray_not_added = $notAdded
            exe_bytes = (Get-Item $tray).Length; floor_bytes = (Get-Item $floor).Length
            wall_tray_first = [math]::Round($wallTray[0], 1); wall_floor_first = [math]::Round($wallFloor[0], 1)
            wall_tray = Get-Stats $wallTray.ToArray(); wall_floor = Get-Stats $wallFloor.ToArray()
        }
        foreach ($k in $cols.Keys) { $row[$k] = Get-Stats $cols[$k].ToArray() }
        $obj = [pscustomobject]$row
        Save-Result ([pscustomobject]@{ scenario = 'startup'; label = $Label; trim = [bool]$Trim; at = (Get-Date).ToString('s'); data = $obj })
        "{0,-12} wall tray med/p95/max {1,6:N1}/{2,6:N1}/{3,6:N1} ms (first {4,6:N1}) | floor med {5,6:N1} ms (first {6,6:N1}) | creation->tray med {7,6:N2} ms | ready WS {8,6:N0} KiB private {9,6:N0} KiB | cpu {10,5:N1} ms | not added {11}" -f `
            $v.Name, $obj.wall_tray.median, $obj.wall_tray.p95, $obj.wall_tray.max, $obj.wall_tray_first, $obj.wall_floor.median, $obj.wall_floor_first, `
            $obj.creation_to_tray.median, $obj.ready_ws.median, $obj.ready_private.median, $obj.ready_cpu.median, $notAdded
        "{0,-12} phases (median ms): creation->main {1:N2} | main->window {2:N2} | window->icon {3:N2} | icon->tray {4:N2}" -f `
            '', $obj.creation_to_main.median, $obj.main_to_window.median, $obj.window_to_icon.median, $obj.icon_to_tray.median
    }
}

function Start-Tray([string]$Tag, [hashtable]$ChildEnv) {
    $exe = Join-Path $root "$Dir\s3_tray.exe"
    $report = Join-Path $work "$Tag.txt"
    Remove-Item $report -ErrorAction SilentlyContinue
    $p = Start-App $exe (Get-AppArgs $report) $ChildEnv
    $w = Wait-Window 'TssSpikeMain' $p.Id
    if (-not $w) { throw 'tray window did not appear' }
    [pscustomobject]@{ Process = $p; Hwnd = $w.Handle; Report = $report }
}

function Invoke-Idle {
    $app = Start-Tray 'idle' $null
    try {
        Start-Sleep -Seconds 3
        $a = Get-Snapshot $app.Process
        Start-Sleep -Seconds $IdleSeconds
        $b = Get-Snapshot $app.Process
        $d = Get-Delta $a $b
        Save-Result ([pscustomobject]@{ scenario = 'idle'; label = $Label; dir = $Dir; trim = [bool]$Trim; seconds = $IdleSeconds; at = (Get-Date).ToString('s'); before = $a; after = $b; delta = $d })
        "idle {0}s [{1}{2}]" -f $IdleSeconds, $Dir, $(if ($Trim) { ' --trim' } else { '' })
        "  memory after: working set {0:N0} KiB | private working set {1:N0} KiB | private bytes {2:N0} KiB | threads {3} | handles {4} | modules {5}" -f $b.WorkingSetKiB, $b.PrivateWsKiB, $b.PrivateKiB, $b.Threads, $b.Handles, $b.Modules
        "  delta over window: cycles {0:N0} | cpu {1} ms | ctx switches {2} (threads +{10}/-{11}) | I/O ops r/w/other {3}/{4}/{5} | I/O bytes r/w/other {6}/{7}/{8} | page faults {9}" -f $d.Cycles, $d.CpuMs, $d.CtxSwitches, $d.IoReadOps, $d.IoWriteOps, $d.IoOtherOps, $d.IoReadBytes, $d.IoWriteBytes, $d.IoOtherBytes, $d.PageFaults, $d.ThreadsAppeared, $d.ThreadsGone
    }
    finally { Stop-App $app.Process $app.Hwnd }
}

function Invoke-Settings {
    $app = Start-Tray 'settings' $null
    try {
        Start-Sleep -Seconds 3
        $base = Get-Snapshot $app.Process
        $cycles = New-Object System.Collections.Generic.List[object]
        for ($i = 1; $i -le 10; $i++) {
            $t0 = [Diagnostics.Stopwatch]::StartNew()
            Send-Command $app.Hwnd $CMD_SETTINGS
            $w = Wait-Window 'TssSpikeSettings' $app.Process.Id
            if (-not $w) { throw 'settings window did not appear' }
            $appearMs = $t0.Elapsed.TotalMilliseconds
            Start-Sleep -Milliseconds 600
            $open = Get-Snapshot $app.Process
            [void][TssNative]::PostMessageW($w.Handle, $WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero)
            $goneMs = Wait-Window 'TssSpikeSettings' $app.Process.Id -Gone
            Start-Sleep -Milliseconds 600
            $closed = Get-Snapshot $app.Process
            $cycles.Add([pscustomobject]@{ n = $i; appear_ms = [math]::Round($appearMs, 1); gone_ms = [math]::Round([double]$goneMs, 1); open = $open; closed = $closed })
        }
        $last = $cycles[$cycles.Count - 1]
        $second = $cycles[1]
        Stop-App $app.Process $app.Hwnd
        $r = Read-Report $app.Report
        $inApp = New-Object System.Collections.Generic.List[double]
        for ($i = 1; $i -le 10; $i++) {
            $v = $r["settings.open${i}_ms"]
            if ($v) { $inApp.Add([double]::Parse($v, [Globalization.CultureInfo]::InvariantCulture)) }
        }
        Save-Result ([pscustomobject]@{ scenario = 'settings'; label = $Label; dir = $Dir; trim = [bool]$Trim; at = (Get-Date).ToString('s'); base = $base; cycles = $cycles; in_app_open_ms = $inApp })
        "settings x10 [{0}{1}]" -f $Dir, $(if ($Trim) { ' --trim' } else { '' })
        "  base:    WS {0,6:N0} KiB | private WS {1,6:N0} | private {2,6:N0} | threads {3} | handles {4} | modules {5}" -f $base.WorkingSetKiB, $base.PrivateWsKiB, $base.PrivateKiB, $base.Threads, $base.Handles, $base.Modules
        foreach ($c in @($cycles[0], $cycles[1], $last)) {
            "  cycle {0,2}: window exists after {1,5:N1} ms (external) | open: WS {2,6:N0} private WS {3,6:N0} private {4,6:N0} threads {5} handles {6} modules {7} | closed: WS {8,6:N0} private WS {9,6:N0} private {10,6:N0} threads {11} handles {12} modules {13}" -f `
                $c.n, $c.appear_ms, $c.open.WorkingSetKiB, $c.open.PrivateWsKiB, $c.open.PrivateKiB, $c.open.Threads, $c.open.Handles, $c.open.Modules, `
                $c.closed.WorkingSetKiB, $c.closed.PrivateWsKiB, $c.closed.PrivateKiB, $c.closed.Threads, $c.closed.Handles, $c.closed.Modules
        }
        if ($inApp.Count -ge 2) {
            $rest = Get-Stats ([double[]]@($inApp | Select-Object -Skip 1))
            "  in-app open->painted ms (create + show + paint + focus): first {0:N1} | cycles 2-10 median {1:N1} p95 {2:N1} max {3:N1}" -f $inApp[0], $rest.median, $rest.p95, $rest.max
        }
        "  one-time warm-up (base -> cycle 1 closed): private {0:N0} KiB | handles {1} | modules {2}" -f ($cycles[0].closed.PrivateKiB - $base.PrivateKiB), ($cycles[0].closed.Handles - $base.Handles), ($cycles[0].closed.Modules - $base.Modules)
        "  per-cycle drift (cycle 2 closed -> cycle 10 closed): private {0:N0} KiB | handles {1} | modules {2}" -f ($last.closed.PrivateKiB - $second.closed.PrivateKiB), ($last.closed.Handles - $second.closed.Handles), ($last.closed.Modules - $second.closed.Modules)
    }
    finally { if ($app -and -not $app.Process.HasExited) { Stop-App $app.Process $app.Hwnd } }
}

function Invoke-Poll {
    $token = Get-Token
    $childEnv = if ($token) { @{ TSS_SPIKE_TOKEN = $token } } else { $null }
    $app = Start-Tray 'poll' $childEnv
    try {
        Start-Sleep -Seconds 3
        $p0 = Get-Snapshot $app.Process
        $sw = [Diagnostics.Stopwatch]::StartNew()
        Send-Command $app.Hwnd $CMD_POLL
        Start-Sleep -Seconds 8
        $p1 = Get-Snapshot $app.Process
        Start-Sleep -Seconds 30
        $p2 = Get-Snapshot $app.Process
        $quiet = Get-Delta $p1 $p2
        $during = Get-Delta $p0 $p1
        Stop-App $app.Process $app.Hwnd
        $r = Read-Report $app.Report
        $keys = @($r.Keys | Where-Object { $_ -like 'poll1.*' } | Sort-Object)
        Save-Result ([pscustomobject]@{ scenario = 'poll'; label = $Label; dir = $Dir; auth = $Auth; trim = [bool]$Trim; at = (Get-Date).ToString('s'); before = $p0; after = $p1; settled = $p2; during = $during; quiet = $quiet; report = ($keys | ForEach-Object { "$_=$($r[$_])" }) })
        "poll [{0}, auth {1}{2}]" -f $Dir, $Auth, $(if ($Trim) { ', --trim' } else { '' })
        foreach ($k in $keys) { "  {0}={1}" -f $k, $r[$k] }
        "  before:  WS {0,6:N0} | private WS {1,6:N0} | private {2,6:N0} | threads {3} | handles {4} | modules {5}" -f $p0.WorkingSetKiB, $p0.PrivateWsKiB, $p0.PrivateKiB, $p0.Threads, $p0.Handles, $p0.Modules
        "  +8 s:    WS {0,6:N0} | private WS {1,6:N0} | private {2,6:N0} | threads {3} | handles {4} | modules {5}" -f $p1.WorkingSetKiB, $p1.PrivateWsKiB, $p1.PrivateKiB, $p1.Threads, $p1.Handles, $p1.Modules
        "  +38 s:   WS {0,6:N0} | private WS {1,6:N0} | private {2,6:N0} | threads {3} | handles {4} | modules {5}" -f $p2.WorkingSetKiB, $p2.PrivateWsKiB, $p2.PrivateKiB, $p2.Threads, $p2.Handles, $p2.Modules
        "  during poll (8 s):  cycles {0:N0} | cpu {1} ms | ctx {2} (threads +{10}/-{11}) | I/O ops r/w/other {3}/{4}/{5} | I/O bytes r/w/other {6}/{7}/{8} | page faults {9}" -f $during.Cycles, $during.CpuMs, $during.CtxSwitches, $during.IoReadOps, $during.IoWriteOps, $during.IoOtherOps, $during.IoReadBytes, $during.IoWriteBytes, $during.IoOtherBytes, $during.PageFaults, $during.ThreadsAppeared, $during.ThreadsGone
        "  quiet after (30 s): cycles {0:N0} | cpu {1} ms | ctx {2} (threads +{10}/-{11}) | I/O ops r/w/other {3}/{4}/{5} | I/O bytes r/w/other {6}/{7}/{8} | page faults {9}" -f $quiet.Cycles, $quiet.CpuMs, $quiet.CtxSwitches, $quiet.IoReadOps, $quiet.IoWriteOps, $quiet.IoOtherOps, $quiet.IoReadBytes, $quiet.IoWriteBytes, $quiet.IoOtherBytes, $quiet.PageFaults, $quiet.ThreadsAppeared, $quiet.ThreadsGone
    }
    finally {
        if ($app -and -not $app.Process.HasExited) { Stop-App $app.Process $app.Hwnd }
        $token = $null
    }
}

function Invoke-Modules {
    $token = Get-Token
    $childEnv = if ($token) { @{ TSS_SPIKE_TOKEN = $token } } else { $null }
    $app = Start-Tray 'modules' $childEnv
    try {
        Start-Sleep -Seconds 2
        $app.Process.Refresh()
        $idle = @($app.Process.Modules | ForEach-Object { [pscustomobject]@{ Name = $_.ModuleName.ToLowerInvariant(); KiB = [int]($_.ModuleMemorySize / 1KB) } })
        Send-Command $app.Hwnd $CMD_SETTINGS
        [void](Wait-Window 'TssSpikeSettings' $app.Process.Id)
        Start-Sleep -Milliseconds 800
        $app.Process.Refresh()
        $withSettings = @($app.Process.Modules | ForEach-Object { $_.ModuleName.ToLowerInvariant() })
        Send-Command $app.Hwnd $CMD_POLL
        Start-Sleep -Seconds 6
        $app.Process.Refresh()
        $afterPoll = @($app.Process.Modules | ForEach-Object { [pscustomobject]@{ Name = $_.ModuleName.ToLowerInvariant(); KiB = [int]($_.ModuleMemorySize / 1KB) } })
        $idleNames = @($idle | ForEach-Object { $_.Name })
        $settingsAdded = @($withSettings | Where-Object { $idleNames -notcontains $_ })
        $pollAdded = @($afterPoll | Where-Object { ($idleNames -notcontains $_.Name) -and ($settingsAdded -notcontains $_.Name) })
        Save-Result ([pscustomobject]@{ scenario = 'modules'; label = $Label; dir = $Dir; at = (Get-Date).ToString('s'); idle = $idle; settings_added = $settingsAdded; poll_added = $pollAdded })
        "tray at idle loads {0} modules ({1:N0} KiB of image)" -f $idle.Count, ($idle | Measure-Object KiB -Sum).Sum
        "  " + ((@($idle | Sort-Object KiB -Descending | ForEach-Object { "$($_.Name) $($_.KiB)K" })) -join ', ')
        "settings window adds {0}: {1}" -f $settingsAdded.Count, ($settingsAdded -join ', ')
        "poll adds {0} modules: {1}" -f $pollAdded.Count, ((@($pollAdded | ForEach-Object { "$($_.Name) $($_.KiB)K" })) -join ', ')
    }
    finally {
        if ($app -and -not $app.Process.HasExited) { Stop-App $app.Process $app.Hwnd }
        $token = $null
    }
}

function Get-ThreadMap([Diagnostics.Process]$P) {
    $P.Refresh()
    $mods = @($P.Modules | ForEach-Object { [pscustomobject]@{ Name = $_.ModuleName.ToLowerInvariant(); Base = [int64]$_.BaseAddress; Size = [int64]$_.ModuleMemorySize } })
    foreach ($t in $P.Threads) {
        $addr = [int64]$t.StartAddress
        $m = $mods | Where-Object { $addr -ge $_.Base -and $addr -lt ($_.Base + $_.Size) } | Select-Object -First 1
        [pscustomobject]@{
            Id       = $t.Id
            Start    = if ($m) { '{0}+0x{1:X}' -f $m.Name, ($addr - $m.Base) } else { '0x{0:X}' -f $addr }
            State    = [string]$t.ThreadState
            Wait     = if ($t.ThreadState -eq 'Wait') { [string]$t.WaitReason } else { '' }
            Priority = $t.CurrentPriority
        }
    }
}

function Invoke-Threads {
    $token = Get-Token
    $childEnv = if ($token) { @{ TSS_SPIKE_TOKEN = $token } } else { $null }
    $app = Start-Tray 'threads' $childEnv
    try {
        $steps = @(
            @{ Name = 'idle, 3 s after start'; Wait = 3; Poll = $false },
            @{ Name = 'idle, 30 s after start'; Wait = 27; Poll = $false },
            @{ Name = '8 s after a poll'; Wait = 8; Poll = $true },
            @{ Name = ('{0} s after the poll' -f ($SettleSeconds + 8)); Wait = $SettleSeconds; Poll = $false }
        )
        $all = @()
        foreach ($s in $steps) {
            if ($s.Poll) { Send-Command $app.Hwnd $CMD_POLL }
            Start-Sleep -Seconds $s.Wait
            $map = @(Get-ThreadMap $app.Process)
            $snap = Get-Snapshot $app.Process
            $all += [pscustomobject]@{ step = $s.Name; threads = $map; handles = $snap.Handles; modules = $snap.Modules; ws_kib = $snap.WorkingSetKiB; private_ws_kib = $snap.PrivateWsKiB; private_kib = $snap.PrivateKiB }
            "[{0}] threads {1} | handles {2} | modules {3} | WS {4:N0} KiB | private WS {5:N0} KiB | private {6:N0} KiB" -f $s.Name, $map.Count, $snap.Handles, $snap.Modules, $snap.WorkingSetKiB, $snap.PrivateWsKiB, $snap.PrivateKiB
            foreach ($g in ($map | Group-Object Start | Sort-Object Name)) {
                $first = $g.Group[0]
                "    {0,2} x {1}  ({2}{3})" -f $g.Count, $g.Name, $first.State, $(if ($first.Wait) { ' / ' + $first.Wait } else { '' })
            }
        }
        Save-Result ([pscustomobject]@{ scenario = 'threads'; label = $Label; dir = $Dir; at = (Get-Date).ToString('s'); steps = $all })
    }
    finally {
        if ($app -and -not $app.Process.HasExited) { Stop-App $app.Process $app.Hwnd }
        $token = $null
    }
}

function Invoke-ColdStart {
    $coldDir = Join-Path $work 'cold'
    New-Item -ItemType Directory -Force $coldDir | Out-Null
    $rng = New-Object Random
    $rows = @()
    try {
        foreach ($name in 's3_floor', 's3_tray') {
            $src = [IO.File]::ReadAllBytes((Join-Path $root "$Dir\$name.exe"))
            $first = New-Object System.Collections.Generic.List[double]
            $second = New-Object System.Collections.Generic.List[double]
            $retried = 0
            for ($i = 1; $i -le $Runs; $i++) {
                $bytes = New-Object byte[] ($src.Length + 32)
                [Array]::Copy($src, $bytes, $src.Length)
                $tail = New-Object byte[] 32
                $rng.NextBytes($tail)
                [Array]::Copy($tail, 0, $bytes, $src.Length, 32)
                $copy = Join-Path $coldDir ('{0}-{1}.exe' -f $name, $i)
                [IO.File]::WriteAllBytes($copy, $bytes)
                if ($ColdDelaySeconds -gt 0) { Start-Sleep -Seconds $ColdDelaySeconds }
                $appArgs = if ($name -eq 's3_tray') { [string[]]@('--report', (Join-Path $work "cold-$i.txt"), '--exit-after-ready') } else { [string[]]@() }
                foreach ($pass in 1, 2) {
                    $sw = [Diagnostics.Stopwatch]::StartNew()
                    $p = $null
                    # A scanner can still hold the new file open right after the write; keep retrying and count that wait as part of the launch.
                    for ($try = 0; $try -lt 200 -and -not $p; $try++) {
                        try { $p = Start-App $copy $appArgs $null }
                        catch { $retried++; [Threading.Thread]::Sleep(10) }
                    }
                    if (-not $p) { throw "could not start $copy" }
                    [void]$p.WaitForExit(20000)
                    $sw.Stop()
                    if ($pass -eq 1) { $first.Add($sw.Elapsed.TotalMilliseconds) } else { $second.Add($sw.Elapsed.TotalMilliseconds) }
                }
                Remove-Item $copy -Force -ErrorAction SilentlyContinue
            }
            $row = [pscustomobject]@{ exe = $name; bytes = $src.Length; delay_s = $ColdDelaySeconds; start_retries = $retried; first = Get-Stats $first.ToArray(); second = Get-Stats $second.ToArray(); first_all = $first.ToArray(); second_all = $second.ToArray() }
            $rows += $row
            "coldstart {0} (delay {1} s, {2} fresh copies, {3} start retries): first launch median {4:N0} ms (min {5:N0}, p95 {6:N0}, max {7:N0}) | second launch median {8:N1} ms (min {9:N1}, max {10:N1})" -f `
                $name, $ColdDelaySeconds, $Runs, $retried, $row.first.median, $row.first.min, $row.first.p95, $row.first.max, $row.second.median, $row.second.min, $row.second.max
        }
        Save-Result ([pscustomobject]@{ scenario = 'coldstart'; label = $Label; dir = $Dir; at = (Get-Date).ToString('s'); data = $rows })
    }
    finally { Remove-Item $coldDir -Recurse -Force -ErrorAction SilentlyContinue }
}

try {
    $stray = @(Get-Process -Name s3_tray, s3_floor -ErrorAction SilentlyContinue)
    if ($stray.Count -gt 0) { throw "Stray spike processes already running (PIDs $($stray.Id -join ', ')). Close them first." }
    if ($Scenario -in 'startup', 'all') { Invoke-Startup }
    if ($Scenario -in 'idle', 'all') { Invoke-Idle }
    if ($Scenario -in 'settings', 'all') { Invoke-Settings }
    if ($Scenario -in 'poll', 'all') { Invoke-Poll }
    if ($Scenario -in 'modules', 'all') { Invoke-Modules }
    if ($Scenario -in 'threads', 'all') { Invoke-Threads }
    if ($Scenario -in 'coldstart', 'all') { Invoke-ColdStart }
}
finally {
    foreach ($id in $started) {
        $p = Get-Process -Id $id -ErrorAction SilentlyContinue
        if ($p -and ($p.ProcessName -like 's3_*')) { Stop-Process -Id $id -Force }
    }
    Remove-Item (Join-Path $work '*') -ErrorAction SilentlyContinue
}
