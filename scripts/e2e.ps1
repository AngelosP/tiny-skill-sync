<#
.SYNOPSIS
  Runs the real tray app and command line against real GitHub repositories, on throwaway folders, and checks what
  ends up on disk, what the app says, and what it leaves alone.

.DESCRIPTION
  Each scenario makes its own folder in %TEMP% and, where it changes upstream, its own branch of a test repository
  (named e2e-STAMP-SCENARIO; nothing else is ever changed or deleted). Both are removed afterwards (-Keep leaves the
  folders). Needs the network and a signed-in GitHub CLI (gh) that may push to the test repositories.

  Nothing here touches the real skills folder, the real settings or the saved GitHub token. While a scenario runs,
  its tray app shows an icon in the notification area, and the ui scenario opens and moves windows on screen.

  Scenarios (all by default; name some with -Only):
    install       first start: what is installed, how the idle app behaves, how fast it quits
    places        with no TSS_ folders: skills go to %USERPROFILE%\.copilot\skills, beside skills written by hand
    changes       six kinds of upstream change reach the disk through the tray's "Check now"
    local-edit    an edit made while the app runs, then an upstream change to the same skill
    edit-stopped  edits made while the app was not running, including ones that keep the file's size and time
    holds         a skill deleted locally, a skill deleted upstream after being edited, every skill deleted upstream
    unmanaged     folders and files that were there before the app: left alone, or adopted only when identical
    private       a private repository: with the token, without it, and a check that the token never shows
    hostile       a branch full of hostile shapes: nothing may be written outside the skills folder
    names         upstream names with control and direction characters are shown escaped
    failures      a bad network, bad answers, a bad token: the skills folder must not change
    stale         GitHub answering with a tree that is newer than its commit
    empty         a repository added as a whole when its skills are in a subfolder
    ui            screenshots and readings of the settings window and the tray menu

  A summary table is printed at the end and the whole run is saved in bench\out\e2e-STAMP.txt. The exit code is 1
  when a check failed.

.PARAMETER Only
  The names of the scenarios to run.

.PARAMETER Shots
  Where the ui scenario saves its screenshots. Default: bench\out\shots-STAMP.

.PARAMETER Keep
  Keep the throwaway folders, to look at them afterwards.

.PARAMETER SkipBuild
  Use target\release as it is.

.EXAMPLE
  scripts\e2e.ps1
  scripts\e2e.ps1 -Only edit-stopped, hostile
#>
param(
    [string[]]$Only = @(),
    [string]$PublicRepo = 'AngelosP/tiny-skill-sync-fixtures-public',
    [string]$PrivateRepo = 'AngelosP/tiny-skill-sync-fixtures-private',
    [string]$Shots = '',
    [switch]$Keep,
    [switch]$SkipBuild
)

. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\e2e-lib.ps1"
. "$PSScriptRoot\e2e-proxy.ps1"

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$outDir = Join-Path $RepoRoot 'bench\out'
New-Item -ItemType Directory -Force $outDir | Out-Null
$script:LogPath = Join-Path $outDir "e2e-$stamp.txt"
if (-not $Shots) { $Shots = Join-Path $outDir "shots-$stamp" }
$script:Results = New-Object System.Collections.Generic.List[object]
$script:Scenario = ''

# ---------------------------------------------------------------- reporting

function Log([string]$Text = '', [string]$Color = '') {
    if ($Color) { Write-Host $Text -ForegroundColor $Color } else { Write-Host $Text }
    [IO.File]::AppendAllText($script:LogPath, $Text + "`r`n")
}

function Note([string]$Text) { Log "  $Text" 'DarkGray' }

function Check([string]$What, $Ok, [string]$Evidence = '') {
    $pass = [bool]$Ok
    $script:Results.Add([pscustomobject]@{ Scenario = $script:Scenario; Check = $What; Pass = $pass; Evidence = $Evidence })
    $line = '  [{0}] {1}' -f $(if ($pass) { 'PASS' } else { 'FAIL' }), $What
    if ($Evidence) { $line += "  ($Evidence)" }
    Log $line $(if ($pass) { 'Green' } else { 'Red' })
}

# ---------------------------------------------------------------- a scenario's folder, branch and apps

function New-Env([string]$Name, [string]$Repo = $PublicRepo, [string]$From = 'main', [switch]$NoBranch, [switch]$WithProxy) {
    $root = New-LiveRoot 'e2e'
    $c = [pscustomobject]@{
        Name   = $Name
        Repo   = $Repo
        Root   = $root
        Skills = (Join-Path $root 'skills')
        Home   = (Join-Path $root 'home')
        Report = (Join-Path $root 'out\marks.txt')
        Branch = $null
        Ref    = $From
        Tray   = $null
        Proxy  = $null
        Id     = $null
    }
    if (-not $NoBranch) {
        $c.Branch = "e2e-$stamp-$Name"
        $c.Ref = $c.Branch
        New-RunBranch $Repo $From $c.Branch
    }
    if ($WithProxy) { $c.Proxy = Start-ChaosProxy }
    $c
}

function Close-Env($c) {
    if ($null -eq $c) { return }
    try { Stop-LiveTray $c.Tray } catch { Note "could not stop the app: $_" }
    if ($c.Proxy) { try { $c.Proxy.Stop() } catch { } }
    if ($c.Branch) {
        try { Remove-RunBranch $c.Repo $c.Branch } catch { Note "could not delete the branch $($c.Branch): $_" }
    }
    if ($Keep) { Note "kept: $($c.Root)" } else { Remove-LiveRoot $c.Root }
}

function Get-EnvOf($c, [hashtable]$More = @{}) {
    $e = @{}
    if ($c.Proxy) { foreach ($k in (Get-ProxyEnv $c.Proxy).Keys) { $e[$k] = (Get-ProxyEnv $c.Proxy)[$k] } }
    foreach ($k in $More.Keys) { $e[$k] = $More[$k] }
    $e
}

# Runs the command line on the scenario's folder and writes down what it printed.
function Tss($c, [string[]]$TssArgs, [hashtable]$Env = @{}) {
    $r = Invoke-LiveTss $c.Root $TssArgs (Get-EnvOf $c $Env)
    Log ('  > tss {0}   (exit {1})' -f ($TssArgs -join ' '), $r.Code) 'DarkGray'
    foreach ($line in ($r.Text -split "`r?`n")) { if ($line.Trim()) { Log "      $line" 'DarkGray' } }
    $r
}

function Get-Address($c, [string]$Path = 'skills') { "https://github.com/$($c.Repo)/tree/$($c.Ref)/$Path" }

function Add-Repo($c, [string[]]$Options = @('--use-token', '--every', '6h'), [string]$Address = '') {
    if (-not $Address) { $Address = Get-Address $c }
    $r = Tss $c (@('add', $Address) + $Options)
    if ($r.Code -ne 0) { throw "tss add failed: $($r.Text)" }
    if ($r.Text -match "as '([^']+)'") { $c.Id = $Matches[1] } else { throw "tss add did not name the repository: $($r.Text)" }
}

function Start-Tray($c, [hashtable]$Env = @{}) {
    $e = @{ TSS_BENCH_REPORT = $c.Report }
    foreach ($k in $Env.Keys) { $e[$k] = $Env[$k] }
    $c.Tray = Start-LiveTray $c.Root @() (Get-EnvOf $c $e)
    $c.Tray
}

# Windows reports a folder's entries as changed the first time anything lists the folder after a job wrote into it (a
# plain FileSystemWatcher sees the same, with no app running), and the app answers with one read-only look two seconds
# later. Listing the folder here and waiting for that look leaves later steps an app with nothing left to do.
function Wait-Settled($c) {
    [void](Get-ChildItem -LiteralPath $c.Skills -Recurse -Force -ErrorAction SilentlyContinue)
    [void](Wait-Quiet $c.Report 4)
}

# Starts the app on the scenario's branch and waits until the first install is complete and quiet.
function Start-Installed($c) {
    Add-Repo $c
    [void](Start-Tray $c)
    [void](Wait-Live { Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\scripts\hello.ps1') } 'the first install' 90)
    [void](Wait-Live { (Get-MarkCount $c.Report 'job-poll') -ge 1 } 'the first poll to finish' 60)
    Wait-Settled $c
}

function Get-SkillNames($c) {
    @(Get-ChildItem -LiteralPath $c.Skills -Directory -Force -ErrorAction SilentlyContinue | Sort-Object Name | ForEach-Object { $_.Name })
}

# The names of everything directly in a folder, in ordinal order, joined by commas ('' when there is none).
function Get-Entries([string]$Dir) {
    $names = [string[]]@(Get-ChildItem -LiteralPath $Dir -Force -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
    [Array]::Sort($names, [StringComparer]::OrdinalIgnoreCase)
    $names -join ','
}

function Read-Skill($c, [string]$Relative) {
    [IO.File]::ReadAllText((Join-Path $c.Skills $Relative))
}

# What the folder differs in from upstream, for one skill only.
function Compare-Skill($c, [string]$Skill) {
    @((Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills') | Where-Object { $_ -match ": $([regex]::Escape($Skill))/" })
}

# Changes one letter of a file and puts its old last-write time back, so its size and time are as before and only the
# content (and the change time Windows keeps for itself) differ.
function Set-SameSizeEdit([string]$Path) {
    $time = (Get-Item -LiteralPath $Path).LastWriteTimeUtc
    $bytes = [IO.File]::ReadAllBytes($Path)
    for ($i = 0; $i -lt $bytes.Length; $i++) {
        if ($bytes[$i] -ge 97 -and $bytes[$i] -le 121) { $bytes[$i] = $bytes[$i] + 1; break }
    }
    [IO.File]::WriteAllBytes($Path, $bytes)
    (Get-Item -LiteralPath $Path).LastWriteTimeUtc = $time
}

# ---------------------------------------------------------------- install

function Test-Install {
    $c = New-Env 'install'
    try {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        Add-Repo $c
        [void](Start-Tray $c)
        [void](Wait-Live { Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\scripts\hello.ps1') } 'the first install' 90)
        Note ('the first install was on disk {0:N1} s after the app was started' -f $clock.Elapsed.TotalSeconds)
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-poll') -ge 1 } 'the first poll to finish' 60)
        Wait-Settled $c
        $marks = @(Read-Marks $c.Report)
        Note ('app marks: ' + (($marks | ForEach-Object { if ($null -ne $_.TookMs) { '{0} at {1:N0} ms (took {2:N0} ms)' -f $_.Mark, $_.AtMs, $_.TookMs } else { '{0} at {1:N0} ms' -f $_.Mark, $_.AtMs } }) -join ', '))

        $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Branch 'skills'
        Check 'the skills folder equals what upstream lists' ($diff.Count -eq 0) (($diff | Select-Object -First 4) -join '; ')
        Check 'alpha, beta and gamma are installed, nothing else' ((Get-SkillNames $c) -join ',' -eq 'alpha,beta,gamma') ((Get-SkillNames $c) -join ',')
        Check 'no window opened by itself (the settings exist)' ((Find-LiveWindow $c.Tray.Id $SETTINGS_CLASS -Visible) -eq [IntPtr]::Zero)

        $p = $c.Tray
        $p.Refresh()
        $cpu0 = $p.TotalProcessorTime.TotalMilliseconds
        $stamp0 = Get-DiskStamp $c.Skills
        $all0 = @(Read-Marks $c.Report).Count
        Start-Sleep -Seconds 20
        $p.Refresh()
        $cpu = $p.TotalProcessorTime.TotalMilliseconds - $cpu0
        Check ('idle for 20 s used {0:N0} ms of CPU' -f $cpu) ($cpu -lt 100) 'limit 100 ms'
        Check ('idle working set {0:N1} MiB' -f ($p.WorkingSet64 / 1MB)) ($p.WorkingSet64 -lt 15MB) "limit 15 MiB; private $([math]::Round($p.PrivateMemorySize64 / 1MB, 1)) MiB, $($p.HandleCount) handles, $($p.Threads.Count) threads"
        $marksAfter = @(Read-Marks $c.Report)
        $newMarks = @($marksAfter | Select-Object -Skip $all0 | ForEach-Object { '{0} at {1:N0} ms' -f $_.Mark, $_.AtMs })
        Check 'idle: nothing written to the skills folder and no job ran' ((Get-DiskStamp $c.Skills) -eq $stamp0 -and $newMarks.Count -eq 0) ("new jobs: " + ($newMarks -join ', ') + "; skills folder changed: " + ((Get-DiskStamp $c.Skills) -ne $stamp0))

        $quit = [Diagnostics.Stopwatch]::StartNew()
        Stop-LiveTray $p
        Check ('quit took {0:N0} ms' -f $quit.ElapsedMilliseconds) ($p.HasExited -and $p.ExitCode -eq 0 -and $quit.ElapsedMilliseconds -lt 8000) "exit code $($p.ExitCode); limit 8000 ms"
        $r = Tss $c @('status')
        Check 'tss status after the app has gone: up to date, exit 0' ($r.Code -eq 0 -and $r.Text -match 'Up to date') ''
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- places

# Every other scenario names the app's folders with TSS_HOME and TSS_SKILLS_DIR. Here both are removed, so the app
# works its folders out as on a real computer: from %USERPROFILE% and %LOCALAPPDATA%, which point into the throwaway
# folder. A cmd.exe started with the same environment proves that before the app runs.
function Test-Places {
    $c = New-Env 'places' -NoBranch
    try {
        $user = Join-Path $c.Root 'user'
        $local = Join-Path $c.Root 'local'
        $copilot = Join-Path $user '.copilot'
        $skills = Join-Path $copilot 'skills'
        $appDir = Join-Path $local 'tiny-skill-sync'
        $real = @{ TSS_HOME = $null; TSS_SKILLS_DIR = $null; USERPROFILE = $user; LOCALAPPDATA = $local }

        # What a computer that uses Copilot already has: a skill written by hand, and Copilot's own files.
        New-Item -ItemType Directory -Force (Join-Path $skills 'my-own'), $local | Out-Null
        [IO.File]::WriteAllText((Join-Path $skills 'my-own\SKILL.md'), "---`nname: my-own`ndescription: Written by hand.`n---`n`nMine.`n")
        [IO.File]::WriteAllText((Join-Path $copilot 'config.json'), "{}`n")
        $mine = Get-DiskStamp (Join-Path $skills 'my-own')
        $copilotFile = Get-BlobId (Join-Path $copilot 'config.json')

        $seen = Get-LiveChildEnv $c.Root $real
        $safe = $seen['USERPROFILE'] -eq $user -and $seen['LOCALAPPDATA'] -eq $local -and -not $seen.ContainsKey('TSS_HOME') -and -not $seen.ContainsKey('TSS_SKILLS_DIR')
        Check 'a program started this way sees the throwaway profile and no TSS_ folders' $safe "USERPROFILE=$($seen['USERPROFILE']); LOCALAPPDATA=$($seen['LOCALAPPDATA'])"
        if (-not $safe) { throw 'refusing to start the app: it could use the real folders' }

        $r = Tss $c @('status') $real
        $where = ($r.Text -match [regex]::Escape("Skills folder: $skills")) -and ($r.Text -match [regex]::Escape("Settings: $appDir\config.json"))
        Check 'tss status names %USERPROFILE%\.copilot\skills and %LOCALAPPDATA%\tiny-skill-sync' $where ''
        if (-not $where) { throw 'refusing to go on: the app chose other folders' }

        $r = Tss $c @('add', (Get-Address $c), '--every', '6h') $real
        if ($r.Text -match "as '([^']+)'") { $c.Id = $Matches[1] }
        Check 'tss add saves the settings in %LOCALAPPDATA%\tiny-skill-sync' ($r.Code -eq 0 -and $c.Id -and (Test-Path -LiteralPath (Join-Path $appDir 'config.json'))) "exit $($r.Code)"

        [void](Start-Tray $c $real)
        [void](Wait-Live { Test-Path -LiteralPath (Join-Path $skills 'alpha\scripts\hello.ps1') } 'the first install' 90)
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-poll') -ge 1 } 'the first poll to finish' 60)
        [void](Get-ChildItem -LiteralPath $skills -Recurse -Force -ErrorAction SilentlyContinue)
        [void](Wait-Quiet $c.Report 4)

        $diff = @(Compare-WithUpstream $skills $c.Repo $c.Ref 'skills' | Where-Object { $_ -notmatch ': my-own/' })
        Check 'the tray installed every skill in %USERPROFILE%\.copilot\skills, equal to upstream' ($diff.Count -eq 0) (($diff | Select-Object -First 4) -join '; ')
        $names = Get-Entries $skills
        Check 'beside the skill written by hand, and nothing else is in the folder' ($names -eq 'alpha,beta,gamma,my-own') $names
        Check 'the skill written by hand and Copilot''s own file are untouched' ((Get-DiskStamp (Join-Path $skills 'my-own')) -eq $mine -and (Get-BlobId (Join-Path $copilot 'config.json')) -eq $copilotFile) ''
        $around = Get-Entries $copilot
        Check 'the work folder is beside the skills folder: .copilot\.tss-work' ($around -eq '.tss-work,config.json,skills') $around
        $app = Get-Entries $appDir
        Check 'the settings and the record are in %LOCALAPPDATA%\tiny-skill-sync' ((Test-Path -LiteralPath (Join-Path $appDir 'config.json')) -and (Test-Path -LiteralPath (Join-Path $appDir 'state.json'))) $app
        $elsewhere = "user: $(Get-Entries $user); local: $(Get-Entries $local); TSS_SKILLS_DIR: $(Get-Entries $c.Skills); TSS_HOME: $(Get-Entries $c.Home)"
        Check 'nothing was written anywhere else' ($elsewhere -eq 'user: .copilot; local: tiny-skill-sync; TSS_SKILLS_DIR: ; TSS_HOME: ') $elsewhere

        Stop-LiveTray $c.Tray
        $r = Tss $c @('status') $real
        Check 'tss status after the app has gone: up to date, exit 0' ($r.Code -eq 0 -and $r.Text -match 'Up to date') "exit $($r.Code)"

        # The skills folder named in config.json instead, for an install that already exists: the new folder gets its own
        # copies, and the one used before is left exactly as it was.
        $moved = Join-Path $c.Root 'moved\skills'
        $before = Get-DiskStamp $skills
        $cfgFile = Join-Path $appDir 'config.json'
        Set-SkillsDir $cfgFile $moved
        $r = Tss $c @('status') $real
        $told = $r.Text -match [regex]::Escape("Warning: the skills folder changed from $skills to $moved")
        Check 'with skills_dir in config.json, tss status names that folder, has nothing installed there, and says why' ($r.Text -match [regex]::Escape("Skills folder: $moved") -and $r.Text -match 'No skills from it are installed' -and $told) "exit $($r.Code)"
        $r = Tss $c @('check') $real
        $diff = Compare-WithUpstream $moved $c.Repo $c.Ref 'skills'
        $names = Get-Entries $moved
        Check 'a check installs every skill in the new folder, equal to upstream' ($r.Code -eq 0 -and $names -eq 'alpha,beta,gamma' -and $diff.Count -eq 0) "exit $($r.Code); $names; $(($diff | Select-Object -First 4) -join '; ')"
        $around = Get-Entries (Split-Path $moved)
        Check 'with its work folder beside it' ($around -eq '.tss-work,skills') $around
        Check 'the folder used before, with the skill written by hand, is left exactly as it was' ((Get-DiskStamp $skills) -eq $before) ''

        # Back to the default folder while the tray runs, after an edit made there in the meantime: the tray notices the
        # settings changed and starts again with that folder, manages the skills there as it did before, keeps the edit
        # and shows it as one, and leaves the other folder as it is.
        $edited = Join-Path $skills 'beta\SKILL.md'
        [IO.File]::AppendAllText($edited, "`nEdited while the app looked elsewhere.`n")
        $editedText = [IO.File]::ReadAllText($edited)
        $movedStamp = Get-DiskStamp $moved
        [void](Start-Tray $c $real)
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -ge 1 } 'the tray to start' 60)
        [void](Wait-Quiet $c.Report 4)
        $starts = Get-MarkCount $c.Report 'job-startup'
        Set-SkillsDir $cfgFile $null
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -gt $starts } 'a start with the folder named again' 60)
        [void](Wait-Quiet $c.Report 4)
        Stop-LiveTray $c.Tray
        $st = [IO.File]::ReadAllText((Join-Path $appDir 'state.json')) | ConvertFrom-Json
        $managed = @($st.repos.($c.Id).skills | ForEach-Object { $_.name }) -join ','
        $aside = @($st.set_aside | ForEach-Object { $_.skills_dir }) -join ','
        Check 'the tray manages the skills in the folder named again as it did before, and keeps the other folder''s record aside' ($st.skills_dir -eq $skills -and $managed -eq 'alpha,beta,gamma' -and $aside -eq $moved) "skills_dir $($st.skills_dir); managed: $managed; set aside: $aside"
        $r = Tss $c @('status') $real
        Check 'it left the edited one as it is, and tss status shows it as edited' ([IO.File]::ReadAllText($edited) -eq $editedText -and $r.Text -match 'beta: Edited locally') "exit $($r.Code)"
        Check 'the skill written by hand is untouched, and so is the folder the tray left' ((Get-DiskStamp (Join-Path $skills 'my-own')) -eq $mine -and (Get-DiskStamp $moved) -eq $movedStamp) ''
    } finally { Close-Env $c }
}

# Names the skills folder in config.json, or with an empty $Dir takes the name out again.
function Set-SkillsDir([string]$File, [string]$Dir) {
    $cfg = [IO.File]::ReadAllText($File) | ConvertFrom-Json
    if ($Dir) {
        $cfg | Add-Member -NotePropertyName 'skills_dir' -NotePropertyValue $Dir -Force
    } else {
        $cfg.PSObject.Properties.Remove('skills_dir')
    }
    [IO.File]::WriteAllText($File, ($cfg | ConvertTo-Json -Depth 8))
}

function Test-Changes {
    $c = New-Env 'changes'
    try {
        Start-Installed $c
        $stamp0 = Get-DiskStamp $c.Skills
        $ms = Invoke-TrayCheck $c.Tray $c.Report
        Check 'a check with nothing new writes nothing' ((Get-DiskStamp $c.Skills) -eq $stamp0) ('{0:N0} ms' -f $ms)
        $looks0 = Get-MarkCount $c.Report 'job-look'
        for ($n = 1; $n -le 6; $n++) {
            $marker = 'm{0}-{1}' -f $n, [guid]::NewGuid().ToString('N').Substring(0, 6)
            $change = Get-FixtureChange $n $c.Skills $marker
            [void](Push-Upstream $c.Repo $c.Branch $change.Push "live: $($change.What)")
            $pushed = Get-Date
            $tries = 0
            $arrived = $false
            while (-not $arrived -and $tries -lt 4) {
                if ($tries -gt 0) { Start-Sleep -Seconds 5 }
                $tries++
                $ms = Invoke-TrayCheck $c.Tray $c.Report
                $arrived = [bool](& $change.Arrived)
            }
            $took = ((Get-Date) - $pushed).TotalSeconds
            Check "change $n ($($change.What)) reached the disk" $arrived ('{0} check(s); the last took {1:N0} ms; {2:N1} s after the push' -f $tries, $ms, $took)
            $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Branch 'skills'
            Check "after change $n the folder equals upstream" ($diff.Count -eq 0) (($diff | Select-Object -First 4) -join '; ')
        }
        Wait-Settled $c
        $all = @(Read-Marks $c.Report).Count
        Start-Sleep -Seconds 15
        Check 'after the changes the app is idle again: no job ran by itself for 15 s' (@(Read-Marks $c.Report).Count -eq $all)
        Note ('job-look ran {0} time(s) for 6 changes and 1 untouched check' -f ((Get-MarkCount $c.Report 'job-look') - $looks0))
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- local-edit

function Test-LocalEdit {
    $c = New-Env 'local-edit'
    try {
        Start-Installed $c
        $alphaMd = Join-Path $c.Skills 'alpha\SKILL.md'
        $guide = Join-Path $c.Skills 'alpha\references\guide.md'
        $guideBefore = Get-BlobId $guide

        $looks = Get-MarkCount $c.Report 'job-look'
        [IO.File]::AppendAllText($alphaMd, "`nmy own line`n")
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-look') -gt $looks } 'the app to look again after the edit' 20)
        $win = Open-Settings $c.Tray
        $list = Get-Control $win 100
        [TssLive]::Select($list, 0)
        Start-Sleep -Milliseconds 800
        $rows = @(Read-UiList $win 110 2)
        $alphaRow = @($rows | Where-Object { $_[0] -eq 'alpha' }) | Select-Object -First 1
        Check 'the window, without being asked, shows alpha as edited' ($alphaRow -and $alphaRow[1] -match 'Edited locally') (($rows | ForEach-Object { $_ -join ': ' }) -join ' | ')
        Close-Settings $c.Tray

        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nupstream moved on`n" }, @{ Path = 'skills/beta/SKILL.md'; Text = "---`nname: beta`ndescription: beta moved on`n---`n" }) 'live: alpha and beta change')
        $tries = 0
        do { $tries++; [void](Invoke-TrayCheck $c.Tray $c.Report); $betaNew = (Read-Skill $c 'beta\SKILL.md') -match 'moved on'; if (-not $betaNew) { Start-Sleep 5 } } while (-not $betaNew -and $tries -lt 4)
        Check 'the sibling (beta) was updated' $betaNew "$tries check(s)"
        Check 'alpha kept my edit' ((Read-Skill $c 'alpha\SKILL.md') -match 'my own line')
        Check 'alpha was not half updated: its other file is as before' ((Get-BlobId $guide) -eq $guideBefore)
        $r = Tss $c @('status')
        Check 'status says alpha needs attention and offers the way out' ($r.Code -eq 1 -and $r.Text -match 'alpha' -and $r.Text -match "tss resolve $($c.Id) alpha replace") ''

        $r = Tss $c @('resolve', $c.Id, 'alpha', 'replace')
        Check 'resolve replace: ok' ($r.Code -eq 0) ''
        $diff = Compare-Skill $c 'alpha'
        Check 'alpha now equals upstream' ($diff.Count -eq 0) ($diff -join '; ')
        $saved = @(Get-ChildItem -LiteralPath $c.Root -Recurse -File -Force -ErrorAction SilentlyContinue | Where-Object { $_.FullName -notlike "$($c.Skills)\*" -and $_.FullName -notlike "$($c.Root)\bin\*" -and $_.FullName -notlike "$($c.Root)\out\*" } | Select-String -SimpleMatch 'my own line' -List)
        Check 'my replaced edit was saved in a backup' ($saved.Count -ge 1) (($saved | ForEach-Object { $_.Path.Substring($c.Root.Length) }) -join ', ')

        [IO.File]::AppendAllText($alphaMd, "`nsecond edit`n")
        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nupstream moved on again`n" }) 'live: alpha changes again')
        $tries = 0
        do { $tries++; [void](Invoke-TrayCheck $c.Tray $c.Report); $r = Tss $c @('status'); $seen = $r.Text -match 'held|Edited locally'; if (-not $seen) { Start-Sleep 5 } } while (-not $seen -and $tries -lt 4)
        Check 'a second edit and a second upstream change: held again' ((Read-Skill $c 'alpha\SKILL.md') -match 'second edit' -and (Read-Skill $c 'alpha\references\guide.md') -notmatch 'again')
        $r = Tss $c @('resolve', $c.Id, 'alpha', 'keep')
        Check 'resolve keep: ok' ($r.Code -eq 0) ''
        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nupstream a third time`n" }) 'live: alpha a third time')
        [void](Invoke-TrayCheck $c.Tray $c.Report)
        Start-Sleep -Seconds 6
        [void](Invoke-TrayCheck $c.Tray $c.Report)
        Check 'a skill kept as mine is never touched again' ((Read-Skill $c 'alpha\SKILL.md') -match 'second edit' -and (Read-Skill $c 'alpha\references\guide.md') -notmatch 'third')
        $r = Tss $c @('status')
        Check 'status says it is kept as mine, with the way back' ($r.Text -match 'Kept as yours' -and $r.Text -match 'manage') ''
        $r = Tss $c @('resolve', $c.Id, 'alpha', 'manage')
        Check 'resolve manage: ok' ($r.Code -eq 0) ''
        [void](Invoke-TrayCheck $c.Tray $c.Report)
        Wait-Settled $c
        $r = Tss $c @('check')
        Check 'after manage again, the next check does not overwrite my edited alpha, and says why' ($r.Code -eq 1 -and $r.Text -match 'alpha' -and $r.Text -match 'a folder with this name already exists' -and (Read-Skill $c 'alpha\SKILL.md') -match 'second edit') "exit $($r.Code)"
        # Known and documented (the plan, "hold visibility"): that hold is told by the check and shown by the running
        # app, but status, which reads no network, does not list it.
        $r = Tss $c @('status')
        Note ('status afterwards says: ' + (($r.Text -split "`r?`n" | Where-Object { $_ -match 'alpha|Up to date' }) -join ' | '))
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- edit-stopped: the key guarantee

function Test-EditStopped {
    $c = New-Env 'edit-stopped'
    try {
        Start-Installed $c
        Stop-LiveTray $c.Tray
        Check 'the app has quit' $c.Tray.HasExited

        $alphaGuide = Join-Path $c.Skills 'alpha\references\guide.md'
        $betaMd = Join-Path $c.Skills 'beta\SKILL.md'
        $gammaMd = Join-Path $c.Skills 'gamma\SKILL.md'
        $before = @{ guide = Get-Item -LiteralPath $alphaGuide; beta = Get-Item -LiteralPath $betaMd }
        $sizes = @{ guide = $before.guide.Length; beta = $before.beta.Length }
        $times = @{ guide = $before.guide.LastWriteTimeUtc; beta = $before.beta.LastWriteTimeUtc }
        $blobGuide = Get-BlobId $alphaGuide
        $blobBeta = Get-BlobId $betaMd

        # (a) a same-size edit with the time put back, in a skill that upstream does not touch;
        # (b) the same in a skill that upstream does change: only the content tells them apart from "untouched";
        # (c) an added file and a deleted file.
        Set-SameSizeEdit $alphaGuide
        Set-SameSizeEdit $betaMd
        [IO.File]::WriteAllText((Join-Path $c.Skills 'alpha\added-while-stopped.txt'), "mine`n")
        Remove-Item -LiteralPath (Join-Path $c.Skills 'alpha\scripts\hello.ps1')
        $guideNow = Get-Item -LiteralPath $alphaGuide
        $betaNow = Get-Item -LiteralPath $betaMd
        Check 'the same-size edits really kept size and last-write time (the hard case)' ($guideNow.Length -eq $sizes.guide -and $guideNow.LastWriteTimeUtc -eq $times.guide -and $betaNow.Length -eq $sizes.beta -and $betaNow.LastWriteTimeUtc -eq $times.beta) ''
        Check 'and the content really differs' ((Get-BlobId $alphaGuide) -ne $blobGuide -and (Get-BlobId $betaMd) -ne $blobBeta)
        $mineGuide = Get-BlobId $alphaGuide
        $mineBeta = Get-BlobId $betaMd

        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/beta/SKILL.md'; Text = "---`nname: beta`ndescription: upstream beta while stopped`n---`n" }, @{ Path = 'skills/gamma/SKILL.md'; Text = "---`nname: gamma`ndescription: upstream gamma while stopped`n---`n" }) 'live: beta and gamma change while the app is stopped')

        [void](Start-Tray $c)
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -ge 1 } 'the start-up job' 60)
        [void](Wait-Quiet $c.Report)
        $startup = @(Read-Marks $c.Report | Where-Object { $_.Mark -eq 'job-startup' }) | Select-Object -First 1
        Note ('start-up job took {0:N0} ms' -f $startup.TookMs)
        Check 'start-up never touched the network or the files it flagged' ((Get-BlobId $alphaGuide) -eq $mineGuide -and (Get-BlobId $betaMd) -eq $mineBeta -and (Read-Skill $c 'gamma\SKILL.md') -notmatch 'while stopped')

        $win = Open-Settings $c.Tray
        [TssLive]::Select((Get-Control $win 100), 0)
        Start-Sleep -Milliseconds 800
        $rows = @(Read-UiList $win 110 2)
        $byName = @{}
        foreach ($row in $rows) { $byName[$row[0]] = $row[1] }
        Note ('the tray shows, before any check: ' + (($rows | ForEach-Object { $_ -join ': ' }) -join ' | '))
        Check 'the tray flags alpha (same-size edit, added file, deleted file) at start-up' ($byName['alpha'] -match 'Edited locally') $byName['alpha']
        Check 'the tray flags beta (same-size edit) at start-up' ($byName['beta'] -match 'Edited locally') $byName['beta']
        Check 'the tray does not flag gamma, which nobody touched' ($byName['gamma'] -notmatch 'Edited|held') $byName['gamma']
        Close-Settings $c.Tray
        $r = Tss $c @('status')
        Check 'tss status says the same' ($r.Text -match 'alpha: Edited locally' -and $r.Text -match 'beta: Edited locally') ''

        $tries = 0
        do { $tries++; [void](Invoke-TrayCheck $c.Tray $c.Report); $gammaNew = (Read-Skill $c 'gamma\SKILL.md') -match 'while stopped'; if (-not $gammaNew) { Start-Sleep 5 } } while (-not $gammaNew -and $tries -lt 4)
        Check 'a check then updated gamma (nobody touched it)' $gammaNew "$tries check(s)"
        Check 'it left my same-size edit in beta alone, although upstream changed beta' ((Get-BlobId $betaMd) -eq $mineBeta -and (Read-Skill $c 'beta\SKILL.md') -notmatch 'while stopped')
        Check 'it left alpha alone: edit, added file, deleted file' ((Get-BlobId $alphaGuide) -eq $mineGuide -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\added-while-stopped.txt')) -and -not (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\scripts\hello.ps1')))
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- holds

function Test-Holds {
    $c = New-Env 'holds'
    try {
        Add-Repo $c
        $r = Tss $c @('check')
        Check 'installed: exit 0' ($r.Code -eq 0 -and (Get-SkillNames $c).Count -eq 3) ((Get-SkillNames $c) -join ',')

        Remove-Item -LiteralPath (Join-Path $c.Skills 'beta') -Recurse -Force
        $r = Tss $c @('status')
        Check 'a skill I deleted is reported as deleted locally' ($r.Text -match 'beta: Deleted locally') ''
        $r = Tss $c @('check')
        Check 'a check does not bring it back by itself' (-not (Test-Path -LiteralPath (Join-Path $c.Skills 'beta'))) ''
        $r = Tss $c @('resolve', $c.Id, 'beta', 'restore')
        $diff = Compare-Skill $c 'beta'
        Check 'resolve restore brings it back, equal to upstream' ($r.Code -eq 0 -and (Test-Path -LiteralPath (Join-Path $c.Skills 'beta\SKILL.md')) -and $diff.Count -eq 0) ($diff -join '; ')

        [IO.File]::AppendAllText((Join-Path $c.Skills 'gamma\SKILL.md'), "`nmine`n")
        [void](Push-Upstream $c.Repo $c.Branch @(@{ DeleteFolder = 'skills/gamma' }) 'live: gamma is removed upstream')
        $tries = 0
        do { $tries++; $r = Tss $c @('check'); $held = $r.Text -match 'gamma'; if (-not $held) { Start-Sleep 5 } } while (-not $held -and $tries -lt 4)
        Check 'a skill removed upstream but edited by me is held, not deleted' ((Test-Path -LiteralPath (Join-Path $c.Skills 'gamma\SKILL.md')) -and (Read-Skill $c 'gamma\SKILL.md') -match 'mine' -and $r.Text -match 'Removal held') "exit $($r.Code)"
        $r = Tss $c @('resolve', $c.Id, 'gamma', 'keep')
        Check 'resolve keep: ok, and it stays' ($r.Code -eq 0 -and (Test-Path -LiteralPath (Join-Path $c.Skills 'gamma\SKILL.md')))

        [void](Push-Upstream $c.Repo $c.Branch @(@{ DeleteFolder = 'skills/alpha' }, @{ DeleteFolder = 'skills/beta' }) 'live: every skill is gone upstream')
        $tries = 0
        do { $tries++; $r = Tss $c @('check'); $mass = $r.Text -match 'lists no skills at all'; if (-not $mass) { Start-Sleep 5 } } while (-not $mass -and $tries -lt 4)
        Check 'every skill gone upstream: removal is held, nothing deleted' ($mass -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\SKILL.md')) -and (Test-Path -LiteralPath (Join-Path $c.Skills 'beta\SKILL.md'))) "exit $($r.Code)"
        $r = Tss $c @('check', $c.Id, '--delete-all')
        Check 'check --delete-all then removes them, and not the one I keep' (-not (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha'))) ((Get-SkillNames $c) -join ',')
        Check 'the skill I kept as mine is still there' (Test-Path -LiteralPath (Join-Path $c.Skills 'gamma\SKILL.md'))
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- unmanaged

function Test-Unmanaged {
    $c = New-Env 'unmanaged'
    try {
        $mine = Join-Path $c.Skills 'mine-only'
        New-Item -ItemType Directory -Force $mine | Out-Null
        [IO.File]::WriteAllText((Join-Path $mine 'SKILL.md'), "---`nname: mine-only`ndescription: not from any repository`n---`n")
        [IO.File]::WriteAllText((Join-Path $c.Skills 'stray.txt'), "a loose file`n")
        New-Item -ItemType Directory -Force (Join-Path $c.Skills 'beta') | Out-Null
        [IO.File]::WriteAllText((Join-Path $c.Skills 'beta\SKILL.md'), "my own beta, not the repository's`n")
        # gamma: a copy that is exactly what upstream has, so it may be taken over.
        $sha = Invoke-Gh @('api', "repos/$($c.Repo)/git/ref/heads/$($c.Branch)", '--jq', '.object.sha')
        $raw = "https://raw.githubusercontent.com/$($c.Repo)/$sha/skills/gamma/SKILL.md"
        $bytes = (Invoke-WebRequest -UseBasicParsing -Uri $raw).Content
        if ($bytes -is [string]) { $bytes = [Text.Encoding]::UTF8.GetBytes($bytes) }
        New-Item -ItemType Directory -Force (Join-Path $c.Skills 'gamma') | Out-Null
        [IO.File]::WriteAllBytes((Join-Path $c.Skills 'gamma\SKILL.md'), $bytes)
        $before = @{ mine = Get-BlobId (Join-Path $mine 'SKILL.md'); stray = Get-BlobId (Join-Path $c.Skills 'stray.txt'); beta = Get-BlobId (Join-Path $c.Skills 'beta\SKILL.md') }

        Add-Repo $c
        $r = Tss $c @('check')
        Check 'a skill that was not there is installed (alpha)' ((Compare-Skill $c 'alpha').Count -eq 0 -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\SKILL.md')))
        Check 'my own skill and my loose file are untouched' ((Get-BlobId (Join-Path $mine 'SKILL.md')) -eq $before.mine -and (Get-BlobId (Join-Path $c.Skills 'stray.txt')) -eq $before.stray)
        Check 'a different folder with an upstream name is not overwritten (beta)' ((Get-BlobId (Join-Path $c.Skills 'beta\SKILL.md')) -eq $before.beta -and $r.Text -match 'beta') ''
        Check 'the check says so (attention)' ($r.Code -eq 1) "exit $($r.Code)"

        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/gamma/SKILL.md'; Text = "---`nname: gamma`ndescription: gamma once adopted`n---`n" }) 'live: gamma changes')
        $tries = 0
        do { $tries++; [void](Tss $c @('check')); $adopted = (Read-Skill $c 'gamma\SKILL.md') -match 'once adopted'; if (-not $adopted) { Start-Sleep 5 } } while (-not $adopted -and $tries -lt 4)
        Check 'an identical copy was taken over: it follows upstream (gamma)' $adopted "$tries check(s)"

        $r = Tss $c @('remove', $c.Id, '--delete-skills')
        Check 'remove --delete-skills deletes what it installed (alpha, gamma)' (-not (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha')) -and -not (Test-Path -LiteralPath (Join-Path $c.Skills 'gamma'))) ((Get-SkillNames $c) -join ',')
        Check 'and leaves everything that was mine (mine-only, stray.txt, beta)' ((Get-BlobId (Join-Path $mine 'SKILL.md')) -eq $before.mine -and (Get-BlobId (Join-Path $c.Skills 'stray.txt')) -eq $before.stray -and (Get-BlobId (Join-Path $c.Skills 'beta\SKILL.md')) -eq $before.beta)
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- private

# Everything the token must not appear in: every file of the scenarios' folders except the program copies, what the
# proxies logged, what the commands printed, and this run's own log. The token is compared in memory; it is never
# written anywhere, and nothing returned from here contains it.
function Find-TokenLeaks([string]$Token, [object[]]$Envs, [string[]]$Printed) {
    $shape = 'gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}'
    $latin1 = [Text.Encoding]::GetEncoding(28591)
    $texts = New-Object System.Collections.Generic.List[object]
    foreach ($e in $Envs) {
        $programs = (Join-Path $e.Root 'bin') + '\'
        foreach ($f in @(Get-ChildItem -LiteralPath $e.Root -Recurse -File -Force -ErrorAction SilentlyContinue)) {
            if ($f.FullName.StartsWith($programs)) { continue }
            try { $texts.Add(@($f.FullName.Substring($e.Root.Length), $latin1.GetString([IO.File]::ReadAllBytes($f.FullName)))) } catch { }
        }
        if ($e.Proxy) { $texts.Add(@("the proxy log of $($e.Name)", ($e.Proxy.Entries() -join "`n"))) }
    }
    $texts.Add(@('what the commands printed', ($Printed -join "`n")))
    $texts.Add(@('the log of this run', [IO.File]::ReadAllText($script:LogPath)))
    $hits = New-Object System.Collections.Generic.List[string]
    foreach ($t in $texts) {
        if ($Token -and $t[1].Contains($Token)) { $hits.Add("$($t[0]): the token itself") }
        elseif ($t[1] -match $shape) { $hits.Add("$($t[0]): something shaped like a token") }
    }
    , @($hits)
}

function Test-Private {
    $token = (Invoke-Gh @('auth', 'token', '--hostname', 'github.com')).Trim()
    $printed = New-Object System.Collections.Generic.List[string]
    $c = $null
    $d = $null
    try {
        $c = New-Env 'private' $PrivateRepo -NoBranch -WithProxy
        Add-Repo $c
        $r = Tss $c @('check')
        $printed.Add($r.Text)
        $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills'
        $have = (Get-SkillNames $c) -join ','
        Check 'with the token the private repository installs, equal to upstream' ($r.Code -eq 0 -and $diff.Count -eq 0 -and $have -eq 'alpha,beta,gamma') "exit $($r.Code); $have; $($diff.Count) difference(s)"
        $log = @($c.Proxy.Entries())
        $bare = @($log | Where-Object { $_ -notmatch 'auth=yes' })
        Check 'every request carried the token: the listing, the commit and each download' ($log.Count -gt 0 -and $bare.Count -eq 0) "$($log.Count - $bare.Count) of $($log.Count) requests"
        $r = Tss $c @('check')
        $printed.Add($r.Text)
        Check 'a second check finds nothing to do, and GitHub answered "not modified"' ($r.Code -eq 0 -and @($c.Proxy.Entries() | Where-Object { $_ -match '-> 304 ' }).Count -ge 1) "exit $($r.Code)"

        $d = New-Env 'private-anon' $PrivateRepo -NoBranch -WithProxy
        Add-Repo $d @('--every', '6h')
        $before = Get-DiskStamp $d.Skills
        $r = Tss $d @('check')
        $printed.Add($r.Text)
        Check 'without the token the repository is not found, and the app says it may be private' ($r.Code -ne 0 -and $r.Text -match 'could not find' -and $r.Text -match "private and 'use token' is off") "exit $($r.Code)"
        Check 'and nothing was installed' ((Get-SkillNames $d).Count -eq 0 -and (Get-DiskStamp $d.Skills) -eq $before)
        $anon = @($d.Proxy.Entries())
        Check 'and no request carried a token' ($anon.Count -gt 0 -and @($anon | Where-Object { $_ -match 'auth=yes' }).Count -eq 0) "$($anon.Count) request(s)"

        $leaks = Find-TokenLeaks $token @($c, $d) $printed.ToArray()
        Check 'the token is nowhere: not in the settings, the saved state, the output, the proxy logs or this log' ($leaks.Count -eq 0) ($leaks -join '; ')
    } finally { Close-Env $c; Close-Env $d }
}

# ---------------------------------------------------------------- hostile

function Test-Hostile {
    $c = New-Env 'hostile' $PublicRepo 'hostile' -NoBranch
    try {
        Add-Repo $c
        $r = Tss $c @('check', '--dry-run')
        Check 'a dry run installs nothing' (@(Get-ChildItem -LiteralPath $c.Skills -Force).Count -eq 0) "exit $($r.Code)"

        $first = Tss $c @('check')
        $entries = @(Get-ChildItem -LiteralPath $c.Skills -Force | Sort-Object Name | ForEach-Object { $_.Name })
        Check 'only the three ordinary skills are installed (no delta, eta, zeta, epsilon, .hidden, loose.md, not-a-skill)' (($entries -join ',') -eq 'alpha,beta,gamma') ($entries -join ',')
        $diffs = @(foreach ($n in 'alpha', 'beta', 'gamma') { Compare-Skill $c $n })
        Check 'what was installed equals upstream' ($diffs.Count -eq 0) ($diffs -join '; ')
        Check 'the check needs attention (exit 1), as there are skills it refused' ($first.Code -eq 1) "exit $($first.Code)"
        foreach ($bad in 'delta', 'eta', 'zeta', 'epsilon') {
            Check "the check names $bad as refused" ($first.Text -match "(?mi)^\s*!\s+$bad\b") ''
        }

        $allowed = @('home', 'skills', 'bin', 'out', '.tss-work')
        $top = @(Get-ChildItem -LiteralPath $c.Root -Force | ForEach-Object { $_.Name })
        $extra = @($top | Where-Object { $allowed -notcontains $_ })
        Check 'nothing was written outside the skills, settings and work folders' ($extra.Count -eq 0) ('top level: ' + ($top -join ', '))
        $all = @(Get-ChildItem -LiteralPath $c.Root -Recurse -Force -ErrorAction SilentlyContinue)
        $links = @($all | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint })
        Check 'there is no link, junction or submodule anywhere' ($links.Count -eq 0) (($links | ForEach-Object { $_.FullName.Substring($c.Root.Length) }) -join ', ')
        $streams = @($all | ForEach-Object { Get-Item -LiteralPath $_.FullName -Stream * -ErrorAction SilentlyContinue } | Where-Object { $_.Stream -ne ':$DATA' })
        Check 'there is no alternate data stream anywhere' ($streams.Count -eq 0) (($streams | ForEach-Object { "$($_.FileName):$($_.Stream)" }) -join ', ')
        $work = Join-Path $c.Root '.tss-work'
        $left = @(Get-ChildItem -LiteralPath $work -Force -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
        Note ("the work folder holds: " + $(if ($left.Count) { $left -join ', ' } else { 'nothing' }))

        $stampA = Get-DiskStamp $c.Skills
        $second = Tss $c @('check')
        Check 'a second check changes nothing on disk' ((Get-DiskStamp $c.Skills) -eq $stampA) "exit $($second.Code)"
        # Known and documented: a refused skill is announced by the check that lists the repository. GitHub answers the
        # second one "not modified", so it has no listing to refuse anything from. (An open decision in the plan.)
        Note ("the second check, answered 'not modified', says: exit $($second.Code), " + ($second.Text -split "`r?`n" | Select-Object -First 1))
        $full = Tss $c @('check', '--full-listing')
        $named = @('delta', 'eta', 'zeta', 'epsilon' | Where-Object { $full.Text -match "(?mi)^\s*!\s+$_\b" })
        Check 'a full listing names the refused skills again, and still changes nothing' ($full.Code -eq 1 -and $named.Count -eq 4 -and (Get-DiskStamp $c.Skills) -eq $stampA) "exit $($full.Code); named: $($named -join ', ')"
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- names

# True for what must never reach a screen as it is: control characters (except line breaks and tab) and the characters
# that reorder or break text.
function Test-Dangerous([int]$N) {
    if ($N -eq 9 -or $N -eq 10 -or $N -eq 13) { return $false }
    ($N -lt 0x20) -or ($N -ge 0x7f -and $N -le 0x9f) -or (@(0x200e, 0x200f, 0x61c, 0x2028, 0x2029) -contains $N) -or ($N -ge 0x202a -and $N -le 0x202e) -or ($N -ge 0x2066 -and $N -le 0x2069)
}

function Has-Danger([string]$Text) {
    foreach ($ch in $Text.ToCharArray()) { if (Test-Dangerous ([int]$ch)) { return $true } }
    $false
}

# The same text with each of those written out, for a log that can be read.
function Show-Safe([string]$Text) {
    $sb = New-Object Text.StringBuilder
    foreach ($ch in $Text.ToCharArray()) {
        if (Test-Dangerous ([int]$ch)) { [void]$sb.Append('\u{').Append(([int]$ch).ToString('x')).Append('}') } else { [void]$sb.Append($ch) }
    }
    $sb.ToString()
}

function Test-Names {
    $c = New-Env 'names'
    try {
        $rloName = 'rlo-' + [char]0x202E + 'gpj'
        $cmdName = 'sub$(echo PWNED)'
        $escName = 'esc-' + [char]0x1B + '[2Jx'
        [void](Push-Upstream $c.Repo $c.Branch @(
                @{ Path = "skills/$rloName/SKILL.md"; Text = "---`nname: rlo`ndescription: a name with a right-to-left override`n---`n" },
                @{ Path = "skills/$cmdName/SKILL.md"; Text = "---`nname: sub`ndescription: a name that is a command when pasted`n---`n" },
                @{ Path = "skills/$escName/SKILL.md"; Text = "---`nname: esc`n---`n" }) 'live: awkward names')

        Add-Repo $c
        $first = Tss $c @('check')
        $folders = @(Get-ChildItem -LiteralPath $c.Skills -Directory -Force | ForEach-Object { $_.Name })
        $shown = ($folders | ForEach-Object { Show-Safe $_ }) -join ', '
        Check 'the folder with a direction-override character and the folder named like a command are installed' ($folders -contains $rloName -and $folders -contains $cmdName) $shown
        Check 'the check wrote the override character out ( \u{202e} ) and printed no control or direction character itself' ($first.Text -match '\\u\{202e\}' -and -not (Has-Danger $first.Text))
        $escInstalled = @($folders | Where-Object { $_ -like 'esc-*' }).Count
        Check 'the folder with an escape character is refused (Windows would not take it), and shown written out' ($escInstalled -eq 0 -and $first.Text -match 'esc-\\u\{1b\}') "installed: $escInstalled"

        foreach ($f in $folders) { [IO.File]::AppendAllText((Join-Path (Join-Path $c.Skills $f) 'SKILL.md'), "`nmy edit`n") }
        $r = Tss $c @('status')
        $danger = Has-Danger $r.Text
        $pasteable = @($r.Text -split "`r?`n" | Where-Object { $_ -match 'resolve' -and ($_ -match '\$\(' -or $_ -match 'PWNED' -or $_ -match '\\u\{') })
        Check 'status: both edited skills need attention (exit 1)' ($r.Code -eq 1 -and $r.Text -match 'Edited locally') "exit $($r.Code)"
        Check 'status prints no command that contains such a name (pasted, it could run code)' ($pasteable.Count -eq 0 -and $r.Text -match 'not safe to paste') "$($pasteable.Count) such line(s)"
        Check 'status printed no control or direction character' (-not $danger)

        [void](Start-Tray $c)
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -ge 1 } 'the start-up look' 30)
        $win = Open-Settings $c.Tray
        [TssLive]::Select((Get-Control $win 100), 0)
        Start-Sleep -Milliseconds 800
        $rows = @(Read-UiList $win 110 2)
        $text = ($rows | ForEach-Object { $_ -join ': ' }) -join "`n"
        Check 'the settings window lists the skills with the characters written out' ($rows.Count -ge 2 -and $text -match '\\u\{202e\}' -and -not (Has-Danger $text)) (Show-Safe ($text -replace "`n", ' | '))
        $list = Get-Control $win 110
        $details = New-Object System.Collections.Generic.List[string]
        for ($i = 0; $i -lt $rows.Count; $i++) {
            [TssLive]::Select($list, $i)
            Start-Sleep -Milliseconds 400
            $details.Add((Read-UiText $win 111))
        }
        $unsafeDetails = @($details | Where-Object { Has-Danger $_ })
        $mine = @(for ($i = 0; $i -lt $rows.Count; $i++) { if (-not $details[$i].StartsWith("$($rows[$i][0]): ")) { $rows[$i][0] } })
        Check 'and so does the detail of every skill, selected one by one, each showing its own skill' ($details.Count -eq $rows.Count -and $unsafeDetails.Count -eq 0 -and $mine.Count -eq 0) ("$($details.Count) detail(s); $($unsafeDetails.Count) with a control or direction character; not showing their own skill: " + (Show-Safe ($mine -join ', ')))
        Note ('the details, one per skill: ' + (Show-Safe (($details | ForEach-Object { $_ -replace "\s+", ' ' }) -join ' || ')))

        # No command is printed for such a name, so this window is the way to settle that skill.
        $at = @(0..($rows.Count - 1) | Where-Object { $rows[$_][0] -like 'rlo-*' })[0]
        $resolved = Get-MarkCount $c.Report 'job-resolve'
        [TssLive]::Select($list, $at)
        Start-Sleep -Milliseconds 400
        Click-Ui $win 112
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-resolve') -gt $resolved } 'the replace to be done' 30)
        $now = [IO.File]::ReadAllText((Join-Path (Join-Path $c.Skills $rloName) 'SKILL.md'))
        Check '"Replace with upstream" on the override-character skill puts the upstream text back' ($now -notmatch 'my edit' -and $now -match 'right-to-left override') ''
        $saved = @(Get-ChildItem -LiteralPath $c.Root -Recurse -File -Force -ErrorAction SilentlyContinue | Where-Object { $_.FullName -notlike "$($c.Skills)\*" -and $_.FullName -notlike "$($c.Root)\bin\*" -and $_.FullName -notlike "$($c.Root)\out\*" } | Select-String -SimpleMatch 'my edit' -List)
        Check 'and my edit was saved in a backup first' ($saved.Count -ge 1) (($saved | ForEach-Object { Show-Safe $_.Path.Substring($c.Root.Length) }) -join ', ')
        Close-Settings $c.Tray
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- failures

function Test-Failures {
    $c = New-Env 'failures' -WithProxy
    try {
        Add-Repo $c
        $r = Tss $c @('check')
        Check 'installed through the proxy first' ($r.Code -eq 0 -and (Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills').Count -eq 0) "exit $($r.Code)"
        $good = Get-DiskStamp $c.Skills
        $stateFile = Join-Path $c.Home 'state.json'
        $stateBefore = if (Test-Path -LiteralPath $stateFile) { Get-BlobId $stateFile } else { '' }

        # The listing fails. Each answer GitHub (or the network) can give wrongly: the folder must not change, nothing
        # may be deleted, and the app must say what happened.
        $listing = @(
            @{ Mode = 'close'; Word = 'No connection' }, @{ Mode = 'hang:40000'; Word = 'No connection' },
            @{ Mode = '401'; Word = 'Access refused' }, @{ Mode = '403'; Word = 'Access refused' }, @{ Mode = 'badauth'; Word = 'Access refused' },
            @{ Mode = 'limit'; Word = 'Rate limited' }, @{ Mode = '429'; Word = 'Rate limited' },
            @{ Mode = '404'; Word = 'Not found' },
            @{ Mode = '500'; Word = 'GitHub is having problems' }, @{ Mode = '503'; Word = 'GitHub is having problems' },
            @{ Mode = 'garbage'; Word = 'Unexpected answer' }, @{ Mode = 'redirect'; Word = 'Repository moved' })
        foreach ($m in $listing) {
            $c.Proxy.SetMode('api', $m.Mode)
            $r = Tss $c @('check')
            $same = (Get-DiskStamp $c.Skills) -eq $good
            Check "the listing fails ($($m.Mode)): says '$($m.Word)', exits with an error, deletes and changes nothing" ($r.Code -ge 2 -and $r.Text -match [regex]::Escape($m.Word) -and $r.Text -match 'Nothing is deleted because a check failed' -and $same) "exit $($r.Code); folder unchanged: $same"
        }
        $c.Proxy.SetMode('api', 'empty-tree')
        $r = Tss $c @('check')
        $same = (Get-DiskStamp $c.Skills) -eq $good
        Check 'a listing that is suddenly empty deletes nothing, and the app says the removals are held' ($r.Code -ne 0 -and $r.Text -match 'lists no skills at all' -and $same) "exit $($r.Code); folder unchanged: $same"
        $c.Proxy.SetMode('api', 'pass')
        $r = Tss $c @('check')
        Check 'when GitHub answers properly again, the next check is fine' ($r.Code -eq 0 -and (Get-DiskStamp $c.Skills) -eq $good) "exit $($r.Code)"
        $stateAfter = if (Test-Path -LiteralPath $stateFile) { Get-BlobId $stateFile } else { '' }
        Note ('the saved state file after all of that is ' + $(if ($stateAfter -eq $stateBefore) { 'byte for byte what it was' } else { 'rewritten (the check times changed)' }))

        # A download fails, with two skills waiting to change.
        [void](Push-Upstream $c.Repo $c.Branch @(
                @{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nnew words`n" },
                @{ Path = 'skills/beta/SKILL.md'; Text = "---`nname: beta`ndescription: beta is newer`n---`n" }) 'live: two skills change')
        $downloads = @('close', 'hang:40000', '404', '500', 'truncated', 'corrupt')
        foreach ($mode in $downloads) {
            $c.Proxy.SetMode('raw', $mode)
            $r = Tss $c @('check')
            $same = (Get-DiskStamp $c.Skills) -eq $good
            Check "a download fails ($mode): exits with an error, and no skill is half updated" ($r.Code -ne 0 -and $same) "exit $($r.Code); folder unchanged: $same"
        }
        $c.Proxy.SetMode('raw', 'pass')
        $r = Tss $c @('check')
        $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills'
        Check 'once downloads work again, both skills update and the folder equals upstream' ($r.Code -eq 0 -and $diff.Count -eq 0) "exit $($r.Code); $($diff.Count) difference(s)"
        Note ('the proxy answered ' + @($c.Proxy.Entries()).Count + ' request(s) in all')
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- stale

function Test-Stale {
    $c = New-Env 'stale' -WithProxy
    try {
        # From the first answer on, the commit of the branch is the one the install saw, however much is pushed later;
        # the listing is always fresh. This is GitHub's cache being behind for one of its two answers.
        $c.Proxy.SetMode('api', 'freeze:/commits/')
        Add-Repo $c
        $r = Tss $c @('check')
        Check 'installed' ($r.Code -eq 0 -and (Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills').Count -eq 0) "exit $($r.Code)"
        $good = Get-DiskStamp $c.Skills

        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nchanged after the install`n" }) 'live: alpha changes')
        $r = Tss $c @('check')
        $replayed = @($c.Proxy.Entries() | Where-Object { $_ -match 'REPLAYED AN EARLIER ANSWER' })
        Check 'the proxy really gave an old commit together with the new listing' ($replayed.Count -ge 1) "$($replayed.Count) replayed answer(s)"
        $same = (Get-DiskStamp $c.Skills) -eq $good
        Check 'the app noticed (the download did not match the listing): "Changed while checking", nothing changed, exit not 0' ($r.Code -ne 0 -and $r.Text -match 'Changed while checking' -and $same) "exit $($r.Code); folder unchanged: $same"

        $c.Proxy.Thaw()
        $r = Tss $c @('check')
        $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills'
        Check 'once the old answer is gone, the change arrives and the folder equals upstream' ($r.Code -eq 0 -and $diff.Count -eq 0 -and (Read-Skill $c 'alpha\references\guide.md') -match 'changed after the install') "exit $($r.Code); $($diff.Count) difference(s)"
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- empty

function Test-Empty {
    $c = New-Env 'empty' -NoBranch
    try {
        Add-Repo $c @('--use-token', '--every', '6h') "https://github.com/$($c.Repo)"
        $r = Tss $c @('check')
        Check 'a repository added as a whole, with its skills in a subfolder: "No skills found", needs attention' ($r.Code -eq 1 -and $r.Text -match 'No skills found') "exit $($r.Code)"
        Check 'it points at the folder to use, as an address' ($r.Text -match 'tree/main/skills')
        Check 'and the output is plain ASCII' (-not ($r.Text -match '[^\x00-\x7f]'))
        Check 'nothing was installed' (@(Get-ChildItem -LiteralPath $c.Skills -Force).Count -eq 0)

        [void](Start-Tray $c)
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -ge 1 } 'the start-up look' 30)
        $win = Open-Settings $c.Tray
        $rows = @(Read-UiList $win 100 4)
        # The record says the last listing was empty, so this needs no check of its own.
        Check 'after a restart, before any check, the repository list already says "No skills found"' ($rows.Count -ge 1 -and $rows[0][3] -match 'No skills found') (($rows | ForEach-Object { $_ -join ' | ' }) -join ' ;; ')
        [void](Invoke-TrayCheck $c.Tray $c.Report)
        Start-Sleep -Milliseconds 800
        $rows = @(Read-UiList $win 100 4)
        Check 'after "Check now", the repository list says "No skills found"' ($rows.Count -ge 1 -and $rows[0][3] -match 'No skills found') (($rows | ForEach-Object { $_ -join ' | ' }) -join ' ;; ')
        [TssLive]::Select((Get-Control $win 100), 0)
        Start-Sleep -Milliseconds 600
        $detail = Read-UiText $win 111
        Check 'and its detail tells what to do (the address with the folder)' ($detail -match 'tree/main/skills') ($detail -replace "\s+", ' ')
        Close-Settings $c.Tray
    } finally { Close-Env $c }
}

# ---------------------------------------------------------------- ui

# Like Wait-Live, but a condition that does not come true gives $null, so that the scenario can record a failed check
# and go on instead of stopping.
function Wait-Maybe([scriptblock]$Until, [int]$TimeoutSec = 10, [int]$EveryMs = 150) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    do {
        try { $r = & $Until } catch { $r = $null }
        if ($r) { return $r }
        Start-Sleep -Milliseconds $EveryMs
    } while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec)
    $null
}

function Get-Config($c) {
    try { [IO.File]::ReadAllText((Join-Path $c.Home 'config.json')) | ConvertFrom-Json } catch { $null }
}

# A picture of a window of the app, saved in the screenshots folder: a copy of the screen, as the person at the computer
# sees the window. If another window is on top of it, that would be in the picture, so the window is taken with
# PrintWindow instead (which is not to be trusted for what the window has drawn; see TssLive.Capture).
# Returns where the picture is and whether it is a copy of the screen.
function Save-Shot([IntPtr]$Window, [string]$Name) {
    New-Item -ItemType Directory -Force $Shots | Out-Null
    $path = Join-Path $Shots "$Name.png"
    $over = [TssLive]::CoveredBy($Window)
    try {
        if ($over -eq '') { [void][TssLive]::CaptureScreen($Window, $path, 0) }
        else { [void][TssLive]::Capture($Window, $path); Note "${Name}: a window ($over) is on top of this one, so its picture was taken with PrintWindow" }
    } catch { Note "no picture for ${Name}: $($_.Exception.Message)"; return $null }
    [pscustomobject]@{ Path = $path; Screen = ($over -eq '') }
}

# What the window has drawn: nothing is left waiting to be painted, and in the picture the faces of the Add and Close
# buttons (at the middle and the bottom of the window) differ from the colour of the window around them. A part that was
# not drawn would be the colour of the window. A button under the mouse has a bluish face, so the difference in any one
# colour will do.
function Test-Painted($Shot, [IntPtr]$Window, [string]$State) {
    $quiet = Wait-Maybe { if (-not [TssLive]::Pending($Window)) { $true } } 3
    Check "${State}: nothing is left waiting to be painted" ($null -ne $quiet) ('waiting: ' + [TssLive]::Pending($Window))
    if (-not $Shot) { Check "${State}: a picture of the window was taken" $false; return }
    if (-not $Shot.Screen) { Note "${State}: what the window drew was not checked, because something covered it"; return }
    $b = ([TssLive]::Bounds($Window)) -split ','
    $client = ([TssLive]::Client($Window)) -split ','
    $frame = ([int]$b[2] - [int]$b[0] - [int]$client[0]) / 2
    $top = ([int]$b[3] - [int]$b[1]) - [int]$client[1] - $frame
    $layout = @(Get-Layout $Window)
    $bmp = New-Object System.Drawing.Bitmap $Shot.Path
    try {
        $seen = @(foreach ($id in 108, 2) {
            $item = $layout | Where-Object { $_.Id -eq $id } | Select-Object -First 1
            $y = [int]($top + $item.Y + $item.H / 2)
            $face = $bmp.GetPixel([int]($frame + $item.X + 6), $y)
            $around = $bmp.GetPixel([int]($frame + $item.X - 4), $y)
            [pscustomobject]@{ Id = $id; Differs = [math]::Max([math]::Abs($face.R - $around.R), [math]::Max([math]::Abs($face.G - $around.G), [math]::Abs($face.B - $around.B))) }
        })
    } finally { $bmp.Dispose() }
    $bare = @($seen | Where-Object { $_.Differs -lt 6 })
    Check "${State}: the picture shows the whole window drawn, down to the Close button" ($bare.Count -eq 0) (($seen | ForEach-Object { "button $($_.Id) differs from the window by $($_.Differs)" }) -join '; ')
}

# Every control inside the window, and none over another (the combo boxes are made tall for their drop-down lists).
function Test-Layout([IntPtr]$Window, [string]$State) {
    $size = ([TssLive]::Client($Window)) -split ','
    $boxes = @(Get-Layout $Window | Where-Object { $_.Shown -and $_.Class -ne 'ComboBox' })
    $name = { param($b) "$($b.Class)#$($b.Id) at $($b.X),$($b.Y) $($b.W)x$($b.H)" }
    $outside = @($boxes | Where-Object { $_.X -lt 0 -or $_.Y -lt 0 -or $_.X + $_.W -gt [int]$size[0] -or $_.Y + $_.H -gt [int]$size[1] })
    Check "${State}: every control is inside the window" ($outside.Count -eq 0) (($outside | ForEach-Object { & $name $_ }) -join '; ')
    $over = New-Object System.Collections.Generic.List[string]
    for ($i = 0; $i -lt $boxes.Count; $i++) {
        for ($j = $i + 1; $j -lt $boxes.Count; $j++) {
            $a = $boxes[$i]; $b = $boxes[$j]
            if ($a.X -lt $b.X + $b.W -and $b.X -lt $a.X + $a.W -and $a.Y -lt $b.Y + $b.H -and $b.Y -lt $a.Y + $a.H) { $over.Add((& $name $a) + ' over ' + (& $name $b)) }
        }
    }
    Check "${State}: no control is over another" ($over.Count -eq 0) ($over -join '; ')
}

# Tab must go down the window, not jump about: each control it visits is no higher than the one before (give or take
# the height of a row).
function Test-TabOrder([IntPtr]$Window, [string]$State) {
    $tabs = @(Get-Layout $Window | Where-Object { $_.Tab -and $_.Shown -and $_.On })
    $back = @(for ($i = 1; $i -lt $tabs.Count; $i++) { if ($tabs[$i].Y + 12 -lt $tabs[$i - 1].Y) { "$($tabs[$i - 1].Id) then $($tabs[$i].Id)" } })
    $order = ($tabs | ForEach-Object { $_.Id }) -join ' '
    Check "${State}: Tab visits the controls in reading order" ($back.Count -eq 0) ("order $order" + $(if ($back.Count) { '; goes back at ' + ($back -join ', ') } else { '' }))
}

function Test-Ui {
    Add-Type -AssemblyName System.Windows.Forms
    $c = New-Env 'ui' -NoBranch -WithProxy
    try {
        $area = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
        $tray = $null

        # ---- the first start: nothing is set up, so the window opens by itself
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $tray = Start-Tray $c
        $win = Wait-Maybe { $w = Find-LiveWindow $tray.Id $SETTINGS_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 10 25
        $opened = $clock.ElapsedMilliseconds
        Check 'with nothing set up, the settings window opens by itself' ($null -ne $win) "visible $opened ms after the start"
        if ($null -eq $win) { return }
        Start-Sleep -Milliseconds 800
        $b = ([TssLive]::Bounds($win)) -split ','
        Check 'the window is on the screen' ([int]$b[0] -ge $area.Left -and [int]$b[1] -ge $area.Top -and [int]$b[2] -le $area.Right -and [int]$b[3] -le $area.Bottom) ("window $($b -join ',') in the work area $($area.Left),$($area.Top),$($area.Right),$($area.Bottom)")
        $ping = [TssLive]::Ping($win, 2000)
        Check 'the window answers at once' ($ping -ge 0 -and $ping -lt 200) "$ping ms"
        $pic = Save-Shot $win '01-first-start'
        Test-Painted $pic $win 'first start'
        $status = Read-Status $win
        Check 'the status line says what to do' ($status -eq 'Add a repository to start') $status
        $rows = @(Read-UiList $win 100 4)
        Check 'the list of repositories is empty' ($rows.Count -eq 0) "$($rows.Count) row(s)"
        $grey = @(Get-Layout $win | Where-Object { $_.Shown -and -not $_.On } | ForEach-Object { $_.Id })
        Check 'what is done to a repository is greyed out: every, token, enabled, check now, remove' (@(101, 102, 103, 104, 105 | Where-Object { $grey -notcontains $_ }).Count -eq 0) ('greyed: ' + (($grey | Sort-Object) -join ' '))
        $label = Get-Layout $win | Where-Object { $_.Class -eq 'Static' -and [TssLive]::Text($_.Handle) -like 'GitHub token:*' } | Select-Object -First 1
        $token = $(if ($label) { [TssLive]::Text($label.Handle) } else { '' })
        Check 'no token is saved, the window says so, and Clear is greyed out' ($token -eq 'GitHub token: not saved' -and $grey -contains 122) $token
        $folder = Read-UiText $win 123
        Check 'the skills folder shown is the one the app was given' ($folder -eq $c.Skills) $folder
        $default = Read-UiText $win 120
        Check 'the default check interval is 1 day' ($default -eq '1 day') $default
        Test-Layout $win 'first start'
        Test-TabOrder $win 'first start'

        # ---- add a repository through the window, the answers slowed down so that the check can be watched
        $c.Proxy.SetMode('api', 'slow:2500')
        Set-UiText $win 106 (Get-Address $c)
        Click-Control $win 107
        Check 'the Private box ticks when it is clicked' (Get-Ticked $win 107)
        Click-Control $win 108
        $seen = New-Object System.Collections.Generic.List[string]
        $worst = 0
        $watch = [Diagnostics.Stopwatch]::StartNew()
        $done = $false
        $pic = $null
        while ($watch.Elapsed.TotalSeconds -lt 40 -and -not $done) {
            $t = Read-Status $win
            if ($t -and -not $seen.Contains($t)) { $seen.Add($t) }
            if ($t -like 'Checking for updates*' -and -not $pic) { $pic = Save-Shot $win '02-checking' }
            $answer = [TssLive]::Ping($win, 3000)
            $worst = [math]::Max($worst, $(if ($answer -lt 0) { 3000 } else { $answer }))
            $now = @(Read-UiList $win 100 4)
            if ($now.Count -ge 1 -and $now[0][3] -eq 'Up to date') { $done = $true }
            Start-Sleep -Milliseconds 100
        }
        $c.Proxy.SetMode('api', 'pass')
        Check 'while it checked, the status line said "Checking for updates"' (@($seen | Where-Object { $_ -like 'Checking for updates*' }).Count -ge 1) ($seen -join ' / ')
        Check 'the window kept answering while it checked' ($worst -lt 500) "slowest answer $worst ms over $([int]$watch.Elapsed.TotalSeconds) s"
        Check 'the check ends with the repository saying Up to date' $done "after $([int]$watch.Elapsed.TotalSeconds) s"
        $rows = @(Read-UiList $win 100 4)
        Note ('the repository row: ' + (($rows | ForEach-Object { $_ -join ' | ' }) -join ' ;; '))
        Check 'the row names the repository, the default interval, and when it was checked' ($rows.Count -eq 1 -and $rows[0][0] -match [regex]::Escape($c.Repo) -and $rows[0][1] -eq '1 day (default)' -and $rows[0][2] -match 'now|ago') (($rows | ForEach-Object { $_ -join ' | ' }) -join ' ;; ')
        $names = @(Read-UiList $win 110 2 | ForEach-Object { $_[0] })
        Check 'the skills list shows alpha, beta and gamma' (($names -join ',') -eq 'alpha,beta,gamma') ($names -join ',')
        Check 'the skills are on disk, equal to upstream' ((Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills').Count -eq 0)
        Check 'the add box is empty again and Private is unticked' ((Read-UiText $win 106) -eq '' -and -not (Get-Ticked $win 107))
        $on = @(Get-Layout $win | Where-Object { $_.Shown -and $_.On } | ForEach-Object { $_.Id })
        Check 'the new repository is selected: every, token, enabled, check now and remove are on' (@(101, 102, 103, 104, 105 | Where-Object { $on -notcontains $_ }).Count -eq 0) ('on: ' + (($on | Sort-Object) -join ' '))
        Check 'Use my token and Enabled are ticked (Private was ticked)' ((Get-Ticked $win 102) -and (Get-Ticked $win 103))
        $said = Wait-Maybe { $s = Read-Status $win; if ($s -like 'Checked*') { $s } } 8
        Check 'when the check is over, the status line says what it did' ($said -eq 'Checked: skills were updated') $said
        $pic = Save-Shot $win '03-installed'
        Test-Painted $pic $win 'a repository added'
        Test-Layout $win 'a repository added'
        Test-TabOrder $win 'a repository added'

        # ---- the two interval boxes
        Choose-Item $win 101 4
        $row = Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -ge 1 -and $r[0][1] -eq '12 hours') { $r[0] } } 8
        $cfg = Wait-Maybe { $x = Get-Config $c; if ($x -and $x.repos[0].interval -eq '12h') { $x } } 8
        Check 'choosing 12 hours in "Check every" is shown in the list and saved for that repository' ($null -ne $row -and $null -ne $cfg) (($row -join ' | ') + '; saved: ' + $(if ($cfg) { $cfg.repos[0].interval } else { '-' }))
        $next = Wait-Maybe { $s = Read-Status $win; if ($s -like 'Next check in*') { $s } } 8
        Check 'once a setting is changed, the status line says when the next check is' ($null -ne $next) (Read-Status $win)
        Choose-Item $win 120 2
        $cfg = Wait-Maybe { $x = Get-Config $c; if ($x -and $x.default_interval -eq '6h') { $x } } 8
        Check 'choosing 6 hours as the default interval is saved, and the repository keeps its own' ($null -ne $cfg -and $cfg.repos[0].interval -eq '12h') ('default ' + $(if ($cfg) { $cfg.default_interval } else { '-' }))
        Choose-Item $win 101 0
        $row = Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -ge 1 -and $r[0][1] -eq '6 hours (default)') { $r[0] } } 8
        $cfg = Get-Config $c
        Check 'choosing Default for the repository makes it follow the default interval' ($null -ne $row -and -not $cfg.repos[0].interval) (($row -join ' | '))

        # ---- Enabled
        $checks = Get-MarkCount $c.Report 'job-check'
        Click-Control $win 103
        $off = Wait-Maybe { $l = @(Get-Layout $win | Where-Object { $_.Id -eq 104 }); if ($l.Count -ge 1 -and -not $l[0].On) { $true } } 8
        $cfg = Get-Config $c
        Check 'unticking Enabled greys out Check now and is saved' ($off -and $cfg.repos[0].enabled -eq $false)
        Note ('with the only repository paused, the row says: ' + ((@(Read-UiList $win 100 4) | ForEach-Object { $_ -join ' | ' }) -join ' ;; ') + '; the status line says: ' + (Read-Status $win))
        Click-Control $win 103
        $back = Wait-Maybe { $l = @(Get-Layout $win | Where-Object { $_.Id -eq 104 }); if ($l.Count -ge 1 -and $l[0].On) { $true } } 8
        $again = Wait-Maybe { if ((Get-MarkCount $c.Report 'job-check') -gt $checks) { $true } } 30
        Check 'ticking Enabled again turns Check now back on and checks the repository at once' ($back -and $again)
        [void](Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -ge 1 -and $r[0][3] -eq 'Up to date') { $true } } 20)
        Wait-Settled $c

        # ---- a local edit appears by itself
        [IO.File]::AppendAllText((Join-Path $c.Skills 'alpha\SKILL.md'), "`nmy own words`n")
        $edited = Wait-Maybe { $r = @(Read-UiList $win 110 2 | Where-Object { $_[0] -eq 'alpha' -and $_[1] -match 'Edited locally' }); if ($r.Count -ge 1) { $r[0] } } 40
        Check 'an edit made to a skill shows in the list as "Edited locally" without anyone asking' ($null -ne $edited) ($edited -join ' | ')
        $row = Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -ge 1 -and $r[0][3] -match 'Needs attention') { $r[0] } } 10
        Check 'and the repository says it needs attention' ($null -ne $row) ($row -join ' | ')
        $index = [array]::IndexOf(@(Read-UiList $win 110 2 | ForEach-Object { $_[0] }), 'alpha')
        [TssLive]::Select((Get-Control $win 110), $index)
        $detail = Wait-Maybe { $t = Read-UiText $win 111; if ($t -like 'alpha:*') { $t } } 8
        Note ('the detail of the edited skill: ' + $detail)
        $one = Read-UiText $win 112
        $two = Read-UiText $win 113
        $shown = @(Get-Layout $win | Where-Object { $_.Shown -and ($_.Id -eq 112 -or $_.Id -eq 113) })
        Check 'selecting it offers two buttons: Replace with upstream, Keep mine' ($shown.Count -eq 2 -and $one -eq 'Replace with upstream' -and $two -eq 'Keep mine') "$one | $two"
        $fit = Get-EditFit $win 111
        Check 'the detail of the skill is shown in full' ($fit.Lines -le $fit.Visible) "$($fit.Lines) line(s), room for $($fit.Visible)"
        Check 'the detail box can be reached with Tab (so it can be read and scrolled from the keyboard)' $fit.Tab
        $pic = Save-Shot $win '04-skill-edited'
        Test-Painted $pic $win 'a skill selected'
        Test-Layout $win 'a skill selected'
        Test-TabOrder $win 'a skill selected'

        # ---- Remove asks first
        Click-Control $win 105
        $ask = Wait-Maybe { $d = Find-LiveWindow $tray.Id $ASK_CLASS -Visible; if ($d -ne [IntPtr]::Zero) { $d } } 8
        Check 'Remove... asks before it does anything' ($null -ne $ask)
        if ($null -ne $ask) {
            Start-Sleep -Milliseconds 500
            [void](Save-Shot $ask '05-remove-asks')
            $kids = @(Get-Layout $ask)
            Note ('the question: ' + (($kids | ForEach-Object { "#$($_.Id) $($_.Class) '" + [TssLive]::Text($_.Handle) + "'" }) -join ' | '))
            $radios = @($kids | Where-Object { $_.Class -eq 'Button' -and ((($_.Style -band 0xF) -eq 4) -or (($_.Style -band 0xF) -eq 9)) })
            $ticks = @($radios | ForEach-Object { [TssLive]::SendMessageW($_.Handle, 0xF0, [IntPtr]::Zero, [IntPtr]::Zero).ToInt64() })
            Check 'it offers two choices, and the one that keeps the skills installed is chosen to start with' ($radios.Count -eq 2 -and $ticks[0] -eq 1 -and $ticks[1] -eq 0 -and [TssLive]::Text($radios[0].Handle) -like 'Leave*') ('ticks ' + ($ticks -join ','))
            $target = [TssLive]::GetDlgItem($ask, 2)
            if ($target -eq [IntPtr]::Zero) { $target = $kids[0].Handle }
            Send-Key $target $VK_ESCAPE
            $gone = Wait-Maybe { if ((Find-LiveWindow $tray.Id $ASK_CLASS -Visible) -eq [IntPtr]::Zero) { $true } } 5
            Check 'Esc cancels the question; the repository and its skills are still there' ($gone -and @(Read-UiList $win 100 4).Count -eq 1 -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\SKILL.md')))
        }

        # ---- failed checks: what the window says about each. No skill is selected, so that the repository's own text shows.
        [TssLive]::Deselect((Get-Control $win 110), [array]::IndexOf(@(Read-UiList $win 110 2 | ForEach-Object { $_[0] }), 'alpha'))
        $none = Wait-Maybe { if ((Read-UiText $win 111) -notlike 'alpha:*') { $true } } 5
        Check 'a skill that is deselected no longer shows its details' ($null -ne $none) (Read-UiText $win 111)
        $c.Proxy.SetMode('api', 'close')
        Click-Control $win 104
        $row = Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -ge 1 -and $r[0][3] -eq 'No connection') { $r[0] } } 30
        Check 'with the network down, Check now ends with the repository saying "No connection"' ($null -ne $row) ($row -join ' | ')
        $said = Read-Status $win
        $reason = $said -replace ' \(.*\)$', ''
        $room = Get-StatusFit $win $reason
        Check 'the status line gives the reason first, and the reason has room to be read' ($said -like 'The connection was lost*' -and $room.Needed -le $room.Available) "$said ($($room.Needed) of $($room.Available) px)"
        $why = Wait-Maybe { $t = Read-UiText $win 111; if ($t -match 'connection') { $t } } 8
        Check 'with no skill selected, the detail box explains what went wrong' ($null -ne $why) $why
        $fit = Get-EditFit $win 111
        Check 'and shows it in full' ($fit.Lines -le $fit.Visible) "$($fit.Lines) line(s), room for $($fit.Visible)"
        Note ('the Messages box holds: ' + ((Read-UiText $win 126) -replace '\s+', ' '))
        Check 'the skills are still listed and on disk' (@(Read-UiList $win 110 2).Count -eq 3 -and @(Get-SkillNames $c).Count -eq 3)
        $pic = Save-Shot $win '06-no-connection'
        Test-Painted $pic $win 'a failed check'
        Test-Layout $win 'a failed check'
        foreach ($case in @(@('404', 'Not found'), @('redirect', 'Repository moved'))) {
            $c.Proxy.SetMode('api', $case[0])
            Click-Control $win 104
            $status = $case[1]
            $row = Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -ge 1 -and $r[0][3] -eq $status) { $r[0] } } 30
            Check "when GitHub answers $($case[0]), the repository says `"$status`"" ($null -ne $row) ($row -join ' | ')
            Start-Sleep -Milliseconds 400
            $fit = Get-EditFit $win 111
            Note ("the detail ($status): " + (Read-UiText $win 111))
            Check "its explanation is shown in full ($status)" ($fit.Lines -le $fit.Visible) "$($fit.Lines) line(s), room for $($fit.Visible)"
            $said = Read-Status $win
            $room = Get-StatusFit $win ($said -replace ' \([^()]*\)$', '')
            Note ("the status line ($status): $said ($($room.Needed) of $($room.Available) px; cut at its end when longer, the box above has the whole text)")
        }
        $c.Proxy.SetMode('api', 'pass')
        Click-Control $win 104
        # Alpha is still edited, so a repository that is well again needs attention for that; "Checking" would not do.
        $healed = Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -ge 1 -and $r[0][3] -match '^Needs attention') { $r[0] } } 30
        Check 'when GitHub answers again, the next Check now clears the failure, and the edited skill is what the repository needs attention for' ($null -ne $healed) $(if ($healed) { $healed -join ' | ' } else { @(Read-UiList $win 100 4)[0] -join ' | ' })

        # ---- the tray menu
        $menu = Show-TrayMenu $tray
        Check 'the tray menu opens' ($menu -ne [IntPtr]::Zero)
        if ($menu -ne [IntPtr]::Zero) {
            $items = @([TssLive]::MenuItems($menu))
            Check 'the menu offers Open settings (the default), Check now, and Quit after a separator' (($items -join ' | ') -eq 'Open settings (default) | Check now | - | Quit') ($items -join ' | ')
            New-Item -ItemType Directory -Force $Shots | Out-Null
            [void][TssLive]::CaptureScreen($menu, (Join-Path $Shots '07-tray-menu.png'), 24)
            Hide-TrayMenu $tray
            $closed = Wait-Maybe { if ((Find-LiveWindow $tray.Id $MENU_CLASS -Visible) -eq [IntPtr]::Zero) { $true } } 4
            Check 'the menu goes away when it is dismissed' $closed
        }

        # ---- closing, and starting the app a second time
        Send-Key (Get-Control $win 106) $VK_ESCAPE
        $closed = Wait-Maybe { if ((Find-LiveWindow $tray.Id $SETTINGS_CLASS -Visible) -eq [IntPtr]::Zero) { $true } } 5
        Check 'Esc closes the settings window, and the app keeps running' ($closed -and -not $tray.HasExited)
        $second = Start-LiveTray $c.Root @() (Get-EnvOf $c @{ TSS_BENCH_REPORT = $c.Report })
        $ended = $second.WaitForExit(8000)
        if (-not $ended) { Stop-Process -Id $second.Id -Force }
        $win = Wait-Maybe { $w = Find-LiveWindow $tray.Id $SETTINGS_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 8
        Check 'starting the app a second time ends the new copy and opens the settings window of the running one' ($ended -and $null -ne $win) "the second copy ended: $ended"
        if ($null -ne $win) {
            Start-Sleep -Milliseconds 500
            Check 'the reopened window still has the repository' (@(Read-UiList $win 100 4).Count -eq 1)

            # ---- bad input in the add box
            Set-UiText $win 106 'not a repo'
            Click-Control $win 108
            # The click is posted, so the line still shows the last check's "Checked: ..." until the window has answered.
            $says = Wait-Maybe { $s = Read-Status $win; if ($s -like 'Enter a repository*') { $s } } 5
            Check 'an address that is not a repository is refused, with a sentence on the status line' ($null -ne $says -and @(Read-UiList $win 100 4).Count -eq 1) $(if ($says) { $says } else { Read-Status $win })
            Check 'and what was typed stays in the box to be corrected' ((Read-UiText $win 106) -eq 'not a repo')
            Set-UiText $win 106 (Get-Address $c)
            Click-Control $win 108
            $says = Wait-Maybe { $s = Read-Status $win; if ($s -match 'already') { $s } } 5
            Check 'adding the same repository twice is refused, and says so' ($null -ne $says -and @(Read-UiList $win 100 4).Count -eq 1) $says
            [void](Save-Shot $win '08-refused')

            # ---- a repository with no skills at its top: what the window says
            Set-UiText $win 106 "https://github.com/$($c.Repo)"
            Click-Control $win 107
            Click-Control $win 108
            $row = Wait-Maybe { $r = @(Read-UiList $win 100 4); if ($r.Count -eq 2 -and $r[1][3] -match 'No skills found') { $r[1] } } 40
            Check 'a repository with no skills at its top says "No skills found"' ($null -ne $row) ($row -join ' | ')
            $says = Wait-Maybe { $s = Read-Status $win; if ($s -match 'no skills found') { $s } } 5
            Check 'and the status line says so too, not "up to date"' ($null -ne $says) $(if ($says) { $says } else { Read-Status $win })
            $detail = Wait-Maybe { $t = Read-UiText $win 111; if ($t -match 'No skills were found') { $t } } 8
            Note ('the detail: ' + $detail)
            $fit = Get-EditFit $win 111
            Check 'its explanation, which ends with what to do about it, is shown in full' ($fit.Lines -le $fit.Visible) "$($fit.Lines) line(s), room for $($fit.Visible)"
            $pic = Save-Shot $win '09-no-skills-found'
            Test-Painted $pic $win 'a repository without skills'
            Test-Layout $win 'a repository without skills'
        }

        $box = Find-LiveWindow $tray.Id '#32770'
        Check 'the app showed no error box' ($box -eq [IntPtr]::Zero)
        Send-LiveCommand $tray $CMD_QUIT
        Check 'Quit ends the app' ($tray.WaitForExit(5000))
    } finally { Close-Env $c }
}

# ---- more scenarios go above this line

# ---------------------------------------------------------------- run

$scenarios = [ordered]@{
    'install'      = 'Test-Install'
    'places'       = 'Test-Places'
    'changes'      = 'Test-Changes'
    'local-edit'   = 'Test-LocalEdit'
    'edit-stopped' = 'Test-EditStopped'
    'holds'        = 'Test-Holds'
    'unmanaged'    = 'Test-Unmanaged'
    'private'      = 'Test-Private'
    'hostile'      = 'Test-Hostile'
    'names'        = 'Test-Names'
    'failures'     = 'Test-Failures'
    'stale'        = 'Test-Stale'
    'empty'        = 'Test-Empty'
    'ui'           = 'Test-Ui'
}

Use-Cargo
if (-not $SkipBuild) { & "$PSScriptRoot\build.ps1" | Out-Host }
[void](Invoke-Gh @('api', 'user', '--jq', '.login'))

$names = if ($Only.Count) { @($Only | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ }) } else { @($scenarios.Keys) }
foreach ($name in $names) { if (-not $scenarios.Contains($name)) { throw "Unknown scenario '$name'. The scenarios are: $($scenarios.Keys -join ', ')" } }

Log "e2e ${stamp}: $($names -join ', ')" 'Cyan'
foreach ($name in $names) {
    $script:Scenario = $name
    Log ''
    Log "== $name" 'Cyan'
    try { & $scenarios[$name] } catch { Check 'the scenario ran to the end' $false ("$($_.Exception.Message) (line $($_.InvocationInfo.ScriptLineNumber))") }
}

Log ''
Log '== Summary' 'Cyan'
$failed = @($script:Results | Where-Object { -not $_.Pass })
foreach ($group in ($script:Results | Group-Object Scenario)) {
    $bad = @($group.Group | Where-Object { -not $_.Pass }).Count
    Log ('  {0,-14} {1,3} checks, {2} failed' -f $group.Name, $group.Count, $bad) $(if ($bad) { 'Red' } else { 'Green' })
}
if ($failed.Count) {
    Log ''
    Log 'Failed checks:' 'Red'
    foreach ($f in $failed) { Log ('  {0}: {1}  ({2})' -f $f.Scenario, $f.Check, $f.Evidence) 'Red' }
}
Log ''
Log "Saved in $script:LogPath"
exit $(if ($failed.Count) { 1 } else { 0 })
