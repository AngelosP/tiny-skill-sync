<#
.SYNOPSIS
  Runs the release tray app for a long time against a real GitHub repository that changes now and then, and
  records what it costs: memory, handles, threads, CPU and file I/O, sampled all the time.

.DESCRIPTION
  Everything happens in a throwaway folder in %TEMP% (deleted at the end unless -Keep) and on a branch made for the
  run in -Repo (deleted at the end). The repository must be one you can push to, such as the test repositories:
  upstream is changed on a timer (a skill is added, a file is edited, a file is added, a skill is deleted, ...), and
  the script notes how long the app takes to put each change on disk.

  The app polls every -Every (the shortest allowed is 5 minutes). It is expected to:
  * stay at the same memory, handles and threads for the whole run;
  * use no CPU and do no file reads or writes between checks;
  * put each upstream change on disk within one interval.

  Results are saved in bench\out\ (a table of samples and a summary) and the summary is printed.

.PARAMETER Repo
  OWNER/NAME of the repository to change (needs push access through the GitHub CLI). The skills it holds under
  skills\ are installed. Default: the public test repository.

.PARAMETER Minutes
  How long to run. Default 75.

.PARAMETER Every
  The check interval, as in 'tss add --every'. Default 5m.

.PARAMETER ChangeEveryMinutes
  How often upstream changes. The first change comes after about half of this, and none comes in the last 6
  minutes, so the last one has time to arrive. Default 10 (a 75-minute run goes through all six kinds of change).

.PARAMETER SampleSeconds
  How often to measure the app. Default 30.

.PARAMETER Keep
  Keep the throwaway folder, to look at it afterwards.

.EXAMPLE
  scripts\soak.ps1
  scripts\soak.ps1 -Minutes 10 -ChangeEveryMinutes 6
#>
param(
    [string]$Repo = 'AngelosP/tiny-skill-sync-fixtures-public',
    [int]$Minutes = 75,
    [string]$Every = '5m',
    [int]$ChangeEveryMinutes = 10,
    [int]$SampleSeconds = 30,
    [switch]$Keep
)

. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\e2e-lib.ps1"

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$branch = "soak-$stamp"
$outDir = Join-Path $RepoRoot 'bench\out'
$csvPath = Join-Path $outDir "soak-$stamp.csv"
$summaryPath = Join-Path $outDir "soak-$stamp.txt"
$folderAddress = "https://github.com/$Repo/tree/$branch/skills"

function Get-Sample([Diagnostics.Process]$P, [Diagnostics.Stopwatch]$Clock) {
    $P.Refresh()
    [uint64]$cycles = 0
    [void][TssLive]::QueryProcessCycleTime($P.Handle, [ref]$cycles)
    $q = Get-CimInstance -ClassName Win32_PerfRawData_PerfProc_Process -Filter ("IDProcess={0}" -f $P.Id)
    [pscustomobject]@{
        Second     = [int]$Clock.Elapsed.TotalSeconds
        WorkingKiB = [int64]($P.WorkingSet64 / 1KB)
        PrivateKiB = [int64]($P.PrivateMemorySize64 / 1KB)
        Handles    = [int]$q.HandleCount
        Threads    = [int]$q.ThreadCount
        CpuMs      = [double]([uint64]$q.PercentProcessorTime) / 10000.0
        CyclesM    = [int64]($cycles / 1000000)
        ReadOps    = [int64]$q.IOReadOperationsPersec
        WriteOps   = [int64]$q.IOWriteOperationsPersec
        OtherOps   = [int64]$q.IOOtherOperationsPersec
        ReadKiB    = [int64]($q.IOReadBytesPersec / 1KB)
        WriteKiB   = [int64]($q.IOWriteBytesPersec / 1KB)
        PageFaults = [int64]$q.PageFaultsPersec
    }
}

$root = $null
$tray = $null
$branchMade = $false
$samples = New-Object System.Collections.Generic.List[object]
$changes = New-Object System.Collections.Generic.List[object]
$problems = New-Object System.Collections.Generic.List[string]

try {
    & "$PSScriptRoot\build.ps1" | Out-Host
    New-Item -ItemType Directory -Force $outDir | Out-Null
    $root = New-LiveRoot 'soak'
    $skillsDir = Join-Path $root 'skills'
    $report = Join-Path $root 'out\marks.txt'
    Write-Host "== Soak for $Minutes minutes in $root (branch $branch of $Repo)" -ForegroundColor Cyan

    New-RunBranch $Repo 'main' $branch
    $branchMade = $true
    $added = Invoke-LiveTss $root @('add', $folderAddress, '--every', $Every)
    if ($added.Code -ne 0) { throw "tss add failed: $($added.Text)" }

    $clock = [Diagnostics.Stopwatch]::StartNew()
    $tray = Start-LiveTray $root @() @{ TSS_BENCH_REPORT = $report }
    [void](Wait-Live { Test-Path -LiteralPath (Join-Path $skillsDir 'alpha\SKILL.md') } 'the first install' 90)
    Write-Host ("First install after {0:N1} s." -f $clock.Elapsed.TotalSeconds)

    'second,working_kib,private_kib,handles,threads,cpu_ms,cycles_m,read_ops,write_ops,other_ops,read_kib,write_kib,page_faults' | Set-Content $csvPath -Encoding ascii
    $first = Get-Sample $tray $clock
    $samples.Add($first)
    $nextChange = [TimeSpan]::FromMinutes([math]::Max(1, $ChangeEveryMinutes / 2))
    $round = 0
    $waiting = $null
    $end = [TimeSpan]::FromMinutes($Minutes)
    $lastPrint = 0

    while ($clock.Elapsed -lt $end) {
        Start-Sleep -Seconds $SampleSeconds
        if ($tray.HasExited) { $problems.Add("The app exited at $([int]$clock.Elapsed.TotalSeconds) s with code $($tray.ExitCode)."); break }
        $s = Get-Sample $tray $clock
        $samples.Add($s)
        ('{0},{1},{2},{3},{4},{5:N1},{6},{7},{8},{9},{10},{11},{12}' -f $s.Second, $s.WorkingKiB, $s.PrivateKiB, $s.Handles, $s.Threads, $s.CpuMs, $s.CyclesM, $s.ReadOps, $s.WriteOps, $s.OtherOps, $s.ReadKiB, $s.WriteKiB, $s.PageFaults) | Add-Content $csvPath -Encoding ascii

        if ($waiting -and (& $waiting.Arrived)) {
            $waiting.Latency = [int]($clock.Elapsed.TotalSeconds - $waiting.PushedAt)
            Write-Host ("  change {0} ({1}) arrived on disk after {2} s" -f $waiting.N, $waiting.What, $waiting.Latency)
            $waiting = $null
        }
        if (-not $waiting -and $clock.Elapsed -ge $nextChange -and $clock.Elapsed -lt $end - [TimeSpan]::FromMinutes(6)) {
            $round++
            $marker = "round-$round-$(Get-Date -Format 'HHmmss')"
            $c = Get-FixtureChange $round $skillsDir $marker
            [void](Push-Upstream $Repo $branch $c.Push "soak change $round ($($c.What))")
            $waiting = [pscustomobject]@{ N = $round; What = $c.What; PushedAt = [int]$clock.Elapsed.TotalSeconds; Latency = $null; Arrived = $c.Arrived }
            $changes.Add($waiting)
            Write-Host ("  change {0} pushed at {1} s: {2}" -f $round, $waiting.PushedAt, $c.What)
            $nextChange += [TimeSpan]::FromMinutes($ChangeEveryMinutes)
        }
        if ($s.Second - $lastPrint -ge 300) {
            $lastPrint = $s.Second
            Write-Host ("  {0,5} s  ws {1,6} KiB  private {2,6} KiB  handles {3}  threads {4}  cpu {5:N0} ms  writes {6}" -f $s.Second, $s.WorkingKiB, $s.PrivateKiB, $s.Handles, $s.Threads, $s.CpuMs, $s.WriteOps)
        }
    }

    # Let the last change arrive, then look at the result.
    if ($waiting) {
        try {
            [void](Wait-Live { & $waiting.Arrived } "change $($waiting.N)" 420 2000)
            $waiting.Latency = [int]($clock.Elapsed.TotalSeconds - $waiting.PushedAt)
        } catch { $problems.Add("Change $($waiting.N) ($($waiting.What)) never arrived on disk.") }
    }
    $final = Get-Sample $tray $clock
    $samples.Add($final)
    $differences = Compare-WithUpstream $skillsDir $Repo $branch 'skills'
    if ($differences.Count -gt 0) { $problems.Add("The skills folder differs from upstream: $($differences -join '; ')") }
    $marks = @(Read-Marks $report)
    Stop-LiveTray $tray
    if (-not $tray.HasExited) { $problems.Add('The app did not quit.') }

    # ---- summary
    $warm = $samples | Where-Object { $_.Second -ge 120 } | Select-Object -First 1
    if (-not $warm) { $warm = $samples[0] }
    $hours = [math]::Max(0.01, ($final.Second - $warm.Second) / 3600.0)
    $markOps = ($marks | Measure-Object).Count
    $polls = @($marks | Where-Object Mark -eq 'job-poll').Count
    $looks = @($marks | Where-Object Mark -eq 'job-look').Count
    $lat = @($changes | Where-Object { $null -ne $_.Latency } | ForEach-Object { $_.Latency })
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("Soak of $([int]($final.Second / 60)) minutes, check every $Every, $($changes.Count) upstream changes ($Repo, branch $branch)")
    $lines.Add(('Working set   : {0:N0} KiB after warm-up, {1:N0} KiB at the end, largest {2:N0} KiB ({3:N0} KiB per hour)' -f $warm.WorkingKiB, $final.WorkingKiB, ($samples | Measure-Object WorkingKiB -Maximum).Maximum, (($final.WorkingKiB - $warm.WorkingKiB) / $hours)))
    $lines.Add(('Private bytes  : {0:N0} KiB after warm-up, {1:N0} KiB at the end, largest {2:N0} KiB ({3:N0} KiB per hour)' -f $warm.PrivateKiB, $final.PrivateKiB, ($samples | Measure-Object PrivateKiB -Maximum).Maximum, (($final.PrivateKiB - $warm.PrivateKiB) / $hours)))
    $lines.Add(('Handles        : {0} after warm-up, {1} at the end, range {2}..{3}' -f $warm.Handles, $final.Handles, ($samples | Measure-Object Handles -Minimum).Minimum, ($samples | Measure-Object Handles -Maximum).Maximum))
    $lines.Add(('Threads        : {0} after warm-up, {1} at the end, range {2}..{3}' -f $warm.Threads, $final.Threads, ($samples | Measure-Object Threads -Minimum).Minimum, ($samples | Measure-Object Threads -Maximum).Maximum))
    $lines.Add(('CPU time       : {0:N0} ms in total, {1:N0} ms since warm-up ({2:N3} % of one core)' -f $final.CpuMs, ($final.CpuMs - $warm.CpuMs), (($final.CpuMs - $warm.CpuMs) / 10.0 / [math]::Max(1, ($final.Second - $warm.Second)))))
    $lines.Add(('File I/O       : {0} reads ({1:N0} KiB), {2} writes ({3:N0} KiB) since warm-up; the timing marks account for {4} writes' -f ($final.ReadOps - $warm.ReadOps), ($final.ReadKiB - $warm.ReadKiB), ($final.WriteOps - $warm.WriteOps), ($final.WriteKiB - $warm.WriteKiB), $markOps))
    $lines.Add("Jobs          : $polls polls, $looks looks at the disk, $markOps marks in all")
    foreach ($c in $changes) { $lines.Add(('Change {0,-2}      : {1,-20} on disk after {2} s' -f $c.N, $c.What, $(if ($null -eq $c.Latency) { 'NEVER' } else { $c.Latency }))) }
    if ($lat.Count -gt 0) { $lines.Add(('Latency        : longest {0} s (interval {1})' -f ($lat | Measure-Object -Maximum).Maximum, $Every)) }
    foreach ($p in $problems) { $lines.Add("PROBLEM: $p") }
    $lines.Add($(if ($problems.Count -eq 0) { 'No problems found.' } else { "$($problems.Count) problem(s) found." }))
    $lines | Set-Content $summaryPath -Encoding ascii
    Write-Host ''
    $lines | ForEach-Object { Write-Host $_ }
    Write-Host "Samples: $csvPath"
    Write-Host "Summary: $summaryPath"
    if ($problems.Count -gt 0) { $script:failed = $true }
} finally {
    Stop-LiveTray $tray
    if ($branchMade) { try { Remove-RunBranch $Repo $branch } catch { Write-Host "Could not delete branch ${branch}: $_" -ForegroundColor Yellow } }
    if (-not $Keep) { Remove-LiveRoot $root } elseif ($root) { Write-Host "Kept $root" }
}
if ($script:failed) { exit 1 }
