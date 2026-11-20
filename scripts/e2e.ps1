<#
.SYNOPSIS
  Runs the real tray app and command line against real GitHub repositories, on throwaway folders, and checks what
  ends up on disk, what the app says, and what it leaves alone.

.DESCRIPTION
  Each scenario makes its own folder in %TEMP% and, where it changes upstream, its own branch of a test repository
  (named e2e-STAMP-SCENARIO; nothing else is ever changed or deleted). Both are removed afterwards (-Keep leaves the
  folders). Needs the network and a signed-in GitHub CLI (gh) that may push to the test repositories.

  Nothing here touches the real skills folder, the real settings, the saved GitHub token or Windows' startup apps (a
  copy started with TSS_HOME keeps its Start with Windows entry in keys of its own, which the scenarios delete). While a
  scenario runs, its tray app shows an icon in the notification area, and the ui scenario opens and moves windows on
  screen.

  Scenarios (all by default; name some with -Only):
    install       first start: what is installed, how the idle app behaves, how fast it quits
    places        with no TSS_ folders: skills go to %USERPROFILE%\.copilot\skills, beside skills written by hand
    portable      export settings once, then run two copies with separate state and skills folders
    changes       six kinds of upstream change reach the disk through the tray's "Sync now"
    local-edit    an edit made while the app runs, then an upstream change to the same skill
    converged     an edit made here that GitHub has too is marked as in sync; one GitHub does not have stays held
    edit-stopped  edits made while the app was not running, including ones that keep the file's size and time
    holds         a skill deleted locally, a skill deleted upstream after being edited, every skill deleted upstream
    unmanaged     folders and files that were there before the app: left alone, or adopted only when identical
    private       a private repository: found to need the token, without any sign-in, and the token never shows
    hostile       a branch full of hostile shapes: nothing may be written outside the skills folder
    names         upstream names with control and direction characters are shown escaped
    failures      a bad network, bad answers, a bad token: the skills folder must not change
    stale         GitHub answering with a tree that is newer than its commit
    empty         a repository added as a whole when its skills are in a subfolder
    signin        Start with Windows: the entry, a start the way Windows starts it at sign-in, copies in long folders
    ui            the window, its questions and the tray menu, driven and read from outside, with screenshots (it moves
                  the mouse pointer and uses the clipboard, putting both back, and opens a skill's folder in File
                  Explorer once, closing it again)

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
        Tray2  = $null
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
    try { Stop-LiveTray $c.Tray2 } catch { Note "could not stop the second app: $_" }
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

# Runs the command line on the scenario's folder and writes down what it printed. $Exe runs another copy of tss.exe.
function Tss($c, [string[]]$TssArgs, [hashtable]$Env = @{}, [string]$Exe = '') {
    $r = Invoke-LiveTss $c.Root $TssArgs (Get-EnvOf $c $Env) -Exe $Exe
    if ($Exe) { Log "  tss at $Exe" 'DarkGray' }
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
        Check 'no window opened by itself (the settings exist)' ((Find-LiveWindow $c.Tray.Id $WINDOW_CLASS -Visible) -eq [IntPtr]::Zero)

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
        $realStartup = Get-RealStartup
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
        $after = Get-RealStartup
        Check 'Windows'' startup apps were not touched' ($after -eq $realStartup) $after
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

function Test-Portable {
    $c = New-Env 'portable' -NoBranch
    try {
        $realStartup = Get-RealStartup
        $address = Get-Address $c
        $added = Tss $c @('add', $address, '--every', '6h')
        Check 'a fixture repository can be added to the local settings first' ($added.Code -eq 0 -and $added.Text -match "Added .* as '") $added.Text
        if ($added.Code -ne 0) { throw 'refusing to continue: the local settings were not created' }

        $localConfig = Join-Path $c.Home 'config.json'
        $localHash = Get-BlobId $localConfig
        $export = Tss $c @('portable')
        $settings = Join-Path $c.Root "bin\tiny-skill-sync.settings.json"
        $sources = Join-Path $c.Root "bin\tiny-skill-sync.sources.json"
        Check 'tss portable creates the two companion files without changing config.json' ($export.Code -eq 0 -and (Test-Path -LiteralPath $settings) -and (Test-Path -LiteralPath $sources) -and (Get-BlobId $localConfig) -eq $localHash) "exit $($export.Code)"
        $settingsHash = Get-BlobId $settings
        $sourcesHash = Get-BlobId $sources
        $portableStatus = Tss $c @('status')
        Check 'tss status reads the companion files and keeps the machine-local skills override' ($portableStatus.Text -match 'Settings: portable companions beside tss.exe' -and $portableStatus.Text -match [regex]::Escape("Skills folder: $($c.Skills)")) $portableStatus.Text

        $secondHome = Join-Path $c.Root 'machine-2\home'
        $secondSkills = Join-Path $c.Root 'machine-2\skills'
        New-Item -ItemType Directory -Force $secondHome, $secondSkills | Out-Null
        Copy-Item -LiteralPath $localConfig -Destination (Join-Path $secondHome 'config.json')
        $secondReport = Join-Path $c.Root 'out\machine-2-marks.txt'
        $c.Tray = Start-LiveTray $c.Root @() (Get-EnvOf $c @{ TSS_BENCH_REPORT = $c.Report })
        $c.Tray2 = Start-LiveTray $c.Root @() (Get-EnvOf $c @{ TSS_HOME = $secondHome; TSS_SKILLS_DIR = $secondSkills; TSS_BENCH_REPORT = $secondReport })
        $firstSkill = Join-Path $c.Skills 'alpha\scripts\hello.ps1'
        $secondSkill = Join-Path $secondSkills 'alpha\scripts\hello.ps1'
        [void](Wait-Live { Test-Path -LiteralPath $firstSkill } 'the first machine to install from shared sources' 120)
        [void](Wait-Live { Test-Path -LiteralPath $secondSkill } 'the second machine to install from shared sources' 120)

        $firstStateFile = Join-Path $c.Home 'state.json'
        $secondStateFile = Join-Path $secondHome 'state.json'
        $firstState = [IO.File]::ReadAllText($firstStateFile) | ConvertFrom-Json
        $secondState = [IO.File]::ReadAllText($secondStateFile) | ConvertFrom-Json
        $firstFiles = ConvertTo-Json -InputObject (Get-DiskFiles $c.Skills) -Compress
        $secondFiles = ConvertTo-Json -InputObject (Get-DiskFiles $secondSkills) -Compress
        $firstNames = (Get-SkillNames $c) -join ','
        $secondNames = @(Get-ChildItem -LiteralPath $secondSkills -Directory | Sort-Object Name | ForEach-Object { $_.Name }) -join ','
        Check 'both machines install the same repository skills' ($firstNames -eq $secondNames -and $firstFiles -eq $secondFiles) "$firstNames / $secondNames"
        Check 'each machine keeps its own state and skills path' ((Test-Path -LiteralPath $firstStateFile) -and (Test-Path -LiteralPath $secondStateFile) -and $firstState.skills_dir -eq $c.Skills -and $secondState.skills_dir -eq $secondSkills) "first: $($firstState.skills_dir); second: $($secondState.skills_dir)"
        Check 'both local config files and shared companions remain unchanged' ((Get-BlobId $localConfig) -eq $localHash -and (Get-BlobId (Join-Path $secondHome 'config.json')) -eq $localHash -and (Get-BlobId $settings) -eq $settingsHash -and (Get-BlobId $sources) -eq $sourcesHash) ''
        $binEntries = Get-Entries (Join-Path $c.Root 'bin')
        Check 'state and skills are not written beside the shared executables' ($binEntries -eq 'tiny-skill-sync.exe,tiny-skill-sync.settings.json,tiny-skill-sync.sources.json,tss.exe') $binEntries
        Check 'Windows startup entries were not touched' ((Get-RealStartup) -eq $realStartup) ''
    } finally { Close-Env $c }
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
        $win = Open-Window $c.Tray
        Open-Repo $win 0
        $alphaRow = Wait-Maybe { $r = @(Read-Skills $win | Where-Object { $_.Name -eq 'alpha' -and $_.Tag -eq 'Edited on this PC' }); if ($r.Count -ge 1) { $r[0] } } 10
        $rows = @(Read-Skills $win)
        Check 'the window, without being asked, shows alpha as edited, with what changed' ($null -ne $alphaRow -and $alphaRow.Detail -match 'Edited locally') (($rows | ForEach-Object { "$($_.Name): $($_.Tag)" }) -join ' | ')
        Close-Window $c.Tray

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
        # engine.lock is skipped: the app holds it shut while a job runs, such as the look the resolve's change starts.
        $saved = @(Get-ChildItem -LiteralPath $c.Root -Recurse -File -Force -ErrorAction SilentlyContinue | Where-Object { $_.FullName -notlike "$($c.Skills)\*" -and $_.FullName -notlike "$($c.Root)\bin\*" -and $_.FullName -notlike "$($c.Root)\out\*" -and $_.Name -ne 'engine.lock' } | Select-String -SimpleMatch 'my own line' -List)
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

# ---------------------------------------------------------------- converged

# The case that looked like a bug: a skill edited here, with the same edit then pushed to GitHub. The folder equals
# what GitHub has, so the app must stop calling it edited, without writing a byte, and one GitHub does not have must
# stay held.
function Test-Converged {
    $c = New-Env 'converged'
    try {
        Start-Installed $c
        $alphaMd = Join-Path $c.Skills 'alpha\SKILL.md'
        $betaMd = Join-Path $c.Skills 'beta\SKILL.md'
        $gammaMd = Join-Path $c.Skills 'gamma\SKILL.md'

        # Alpha's edit has Windows line endings, which GitHub's copy will not have (it must read GitHub's file to
        # tell). Beta's will be on GitHub byte for byte (the file listing alone tells). Gamma's will not be on GitHub.
        $alphaText = (Read-Skill $c 'alpha\SKILL.md') + "`nmy own line`n"
        $betaText = (Read-Skill $c 'beta\SKILL.md') + "`nmy own line`n"
        $gammaText = (Read-Skill $c 'gamma\SKILL.md') + "`nmy own line`n"
        $looks = Get-MarkCount $c.Report 'job-look'
        [IO.File]::WriteAllText($alphaMd, $alphaText.Replace("`n", "`r`n"))
        [IO.File]::WriteAllText($betaMd, $betaText)
        [IO.File]::WriteAllText($gammaMd, $gammaText)
        $mine = @{ alpha = Get-BlobId $alphaMd; beta = Get-BlobId $betaMd; gamma = Get-BlobId $gammaMd }
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-look') -gt $looks } 'the app to look again after the edits' 20)
        [void](Wait-Quiet $c.Report)
        $r = Tss $c @('status')
        Check 'before anything is pushed, status calls all three edited' ($r.Text -match 'alpha: Edited locally' -and $r.Text -match 'beta: Edited locally' -and $r.Text -match 'gamma: Edited locally') ''

        [void](Push-Upstream $c.Repo $c.Branch @(
            @{ Path = 'skills/alpha/SKILL.md'; Text = $alphaText },
            @{ Path = 'skills/beta/SKILL.md'; Text = $betaText },
            @{ Path = 'skills/gamma/SKILL.md'; Text = $gammaText + "and GitHub's own line`n" }
        ) 'live: the edits made here, pushed too (gamma differently)')
        $tries = 0
        do {
            $tries++
            [void](Invoke-TrayCheck $c.Tray $c.Report)
            $r = Tss $c @('status')
            $settled = $r.Text -notmatch 'alpha: Edited locally' -and $r.Text -notmatch 'beta: Edited locally'
            if (-not $settled) { Start-Sleep 5 }
        } while (-not $settled -and $tries -lt 4)
        Check 'a check marks alpha and beta as in sync: status stops calling them edited' $settled "$tries check(s)"
        Check 'gamma, whose edit GitHub does not have, is still edited, and status asks for attention' ($r.Code -eq 1 -and $r.Text -match 'gamma: Edited locally') "exit $($r.Code)"
        $backups = @(Get-ChildItem -LiteralPath (Join-Path $c.Root '.tss-work') -Directory -Filter 'backup-*' -ErrorAction SilentlyContinue)
        Check 'nothing was written: my three files are exactly as I left them, and no backup was made' ((Get-BlobId $alphaMd) -eq $mine.alpha -and (Get-BlobId $betaMd) -eq $mine.beta -and (Get-BlobId $gammaMd) -eq $mine.gamma -and $backups.Count -eq 0) "$($backups.Count) backup(s)"
        Check 'beta equals upstream byte for byte, and alpha does apart from its line endings' ((Compare-Skill $c 'beta').Count -eq 0 -and (Read-Skill $c 'alpha\SKILL.md').Replace("`r`n", "`n") -eq $alphaText) ''

        $win = Open-Window $c.Tray
        Open-Repo $win 0
        $gammaRow = Wait-Maybe { $g = @(Read-Skills $win | Where-Object { $_.Name -eq 'gamma' -and $_.Tag -eq 'Edited on this PC' }); if ($g.Count -ge 1) { $g[0] } } 10
        $rows = @(Read-Skills $win)
        $tag = @{}
        foreach ($row in $rows) { $tag[$row.Name] = $row.Tag }
        Check 'the window no longer calls alpha or beta edited, and still says so of gamma' ($null -ne $gammaRow -and $tag['alpha'] -match '^(Installed|Up to date)$' -and $tag['beta'] -match '^(Installed|Up to date)$') (($rows | ForEach-Object { "$($_.Name): $($_.Tag)" }) -join ' | ')
        Close-Window $c.Tray

        # Marked as in sync must mean managed again: the next change GitHub makes to alpha is simply installed. An
        # update puts the whole skill back as GitHub has it, so alpha ends up with GitHub's line endings too.
        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nupstream moved on after the catch-up`n" }) 'live: alpha moves on')
        $tries = 0
        do { $tries++; [void](Invoke-TrayCheck $c.Tray $c.Report); $guideNew = (Read-Skill $c 'alpha\references\guide.md') -match 'after the catch-up'; if (-not $guideNew) { Start-Sleep 5 } } while (-not $guideNew -and $tries -lt 4)
        Check 'alpha, in sync now, takes the next upstream change like any other skill' $guideNew "$tries check(s)"
        $alphaDiff = Compare-Skill $c 'alpha'
        Check 'and that update left alpha exactly as GitHub has it' ($alphaDiff.Count -eq 0) ($alphaDiff -join ', ')
        Check 'gamma, still held, was not touched by it' ((Get-BlobId $gammaMd) -eq $mine.gamma) ''
        Wait-Settled $c
        $all = @(Read-Marks $c.Report).Count
        Start-Sleep -Seconds 15
        Check 'the app is idle again: no job ran by itself for 15 s' (@(Read-Marks $c.Report).Count -eq $all)
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
        # (c) a deleted file, and an added file, which is mine and so is never reported.
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

        $win = Open-Window $c.Tray
        Open-Repo $win 0
        $rows = @(Read-Skills $win)
        $byName = @{}
        foreach ($row in $rows) { $byName[$row.Name] = "$($row.Tag): $($row.Detail)" }
        Note ('the window shows, before any check: ' + (($rows | ForEach-Object { "$($_.Name): $($_.Tag)" + $(if ($_.Detail) { " ($($_.Detail))" } else { '' }) }) -join ' | '))
        Check 'the window flags alpha (same-size edit, deleted file) at start-up, and not the file I added' ($byName['alpha'] -match '^Edited on this PC: Edited locally' -and $byName['alpha'] -notmatch 'added-while-stopped') $byName['alpha']
        Check 'the window flags beta (same-size edit) at start-up' ($byName['beta'] -match '^Edited on this PC: Edited locally') $byName['beta']
        Check 'the window does not flag gamma, which nobody touched' ($byName.ContainsKey('gamma') -and $byName['gamma'] -notmatch 'Edited|held') $byName['gamma']
        Close-Window $c.Tray
        $r = Tss $c @('status')
        Check 'tss status says the same' ($r.Text -match 'alpha: Edited locally' -and $r.Text -match 'beta: Edited locally') ''

        $tries = 0
        do { $tries++; [void](Invoke-TrayCheck $c.Tray $c.Report); $gammaNew = (Read-Skill $c 'gamma\SKILL.md') -match 'while stopped'; if (-not $gammaNew) { Start-Sleep 5 } } while (-not $gammaNew -and $tries -lt 4)
        Check 'a check then updated gamma (nobody touched it)' $gammaNew "$tries check(s)"
        Check 'it left my same-size edit in beta alone, although upstream changed beta' ((Get-BlobId $betaMd) -eq $mineBeta -and (Read-Skill $c 'beta\SKILL.md') -notmatch 'while stopped')
        Check 'it left alpha alone: edit, deleted file, and the file I added' ((Get-BlobId $alphaGuide) -eq $mineGuide -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\added-while-stopped.txt')) -and -not (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\scripts\hello.ps1')))
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

        # A file of my own in alpha: it stays when alpha goes, and it does not make alpha an edit.
        [IO.File]::WriteAllText((Join-Path $c.Skills 'alpha\my-notes.md'), "my notes`n")

        # GitHub then adds a file where I had added one: GitHub's goes in, mine is kept as a backup, and the check names
        # it and the backup (and not my other file). A check may first stop while GitHub's servers catch up.
        [IO.File]::WriteAllText((Join-Path $c.Skills 'alpha\references\clash.md'), "my clash`n")
        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/clash.md'; Text = "github's clash`n" }) 'live: alpha gains a file I had added too')
        $tries = 0
        do { $tries++; $r = Tss $c @('check'); $told = $r.Text -match 'references/clash\.md'; if (-not $told) { Start-Sleep 15 } } while (-not $told -and $tries -lt 10)
        $kept = @(Get-ChildItem -LiteralPath (Join-Path $c.Root '.tss-work') -Directory -Filter 'backup-alpha-*' | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'references\clash.md') })
        $backup = if ($kept.Count -eq 1) { $kept[0].FullName } else { '' }
        $mineKept = $backup -and [IO.File]::ReadAllText((Join-Path $backup 'references\clash.md')) -eq "my clash`n"
        Check 'GitHub adds a file I had added: theirs goes in, mine is kept as a backup, and the check names both' ($told -and $mineKept -and (Read-Skill $c 'alpha\references\clash.md') -eq "github's clash`n" -and $r.Text -match ('Warning: alpha: .*' + [regex]::Escape($backup)) -and $r.Text -notmatch 'my-notes') "exit $($r.Code); $($kept.Count) backup(s) with it"
        Check 'my other file stays in alpha' ((Read-Skill $c 'alpha\my-notes.md') -eq "my notes`n")

        [void](Push-Upstream $c.Repo $c.Branch @(@{ DeleteFolder = 'skills/alpha' }, @{ DeleteFolder = 'skills/beta' }) 'live: every skill is gone upstream')
        $tries = 0
        do { $tries++; $r = Tss $c @('check'); $mass = $r.Text -match 'lists no skills at all'; if (-not $mass) { Start-Sleep 5 } } while (-not $mass -and $tries -lt 4)
        Check 'every skill gone upstream: removal is held, nothing deleted' ($mass -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\SKILL.md')) -and (Test-Path -LiteralPath (Join-Path $c.Skills 'beta\SKILL.md'))) "exit $($r.Code)"
        $r = Tss $c @('check', $c.Id, '--delete-all')
        Check 'check --delete-all then removes what GitHub put there, and not the one I keep' (-not (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\SKILL.md')) -and -not (Test-Path -LiteralPath (Join-Path $c.Skills 'beta'))) ("exit $($r.Code); " + ((Get-SkillNames $c) -join ','))
        Check 'the file I added to alpha stays, alone in its folder' ((Get-Entries (Join-Path $c.Skills 'alpha')) -eq 'my-notes.md' -and (Read-Skill $c 'alpha\my-notes.md') -eq "my notes`n") (Get-Entries (Join-Path $c.Skills 'alpha'))
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
        # gamma: a copy that is exactly what upstream has, so it may be taken over, with a file of my own beside it.
        $sha = Wait-Live { try { Get-PublicCommit $c.Repo $c.Branch } catch { $null } } 'the public fixture branch' 90
        $raw = "https://raw.githubusercontent.com/$($c.Repo)/$sha/skills/gamma/SKILL.md"
        $bytes = (Invoke-WebRequest -UseBasicParsing -Uri $raw).Content
        if ($bytes -is [string]) { $bytes = [Text.Encoding]::UTF8.GetBytes($bytes) }
        New-Item -ItemType Directory -Force (Join-Path $c.Skills 'gamma') | Out-Null
        [IO.File]::WriteAllBytes((Join-Path $c.Skills 'gamma\SKILL.md'), $bytes)
        [IO.File]::WriteAllText((Join-Path $c.Skills 'gamma\my-extra.md'), "my extra`n")
        # A whole old copy, an edited old copy, and more history than one quick slice can search.
        [void](Push-Upstream $c.Repo $c.Branch @(
            @{ Path = 'skills/older-edited/SKILL.md'; Text = "old edited fixture`n" },
            @{ Path = 'skills/slow/SKILL.md'; Text = "old slow fixture`n" }
        ) 'live: historical copies')
        New-Item -ItemType Directory -Force (Join-Path $c.Skills 'older-edited'), (Join-Path $c.Skills 'slow') | Out-Null
        [IO.File]::WriteAllText((Join-Path $c.Skills 'older-edited\SKILL.md'), "old edited fixture`nmy edit`n")
        [IO.File]::WriteAllText((Join-Path $c.Skills 'slow\SKILL.md'), "old slow fixture`n")
        $newGamma = "---`nname: gamma`ndescription: historical gamma`n---`n"
        $head = Push-Upstream $c.Repo $c.Branch @(
            @{ Path = 'skills/gamma/SKILL.md'; Text = $newGamma },
            @{ Path = 'skills/older-edited/SKILL.md'; Text = "new edited fixture`n" },
            @{ Path = 'skills/slow/SKILL.md'; Text = "slow version 1`n" }
        ) 'live: advance historical copies'
        for ($version = 2; $version -le 9; $version++) {
            $head = Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/slow/SKILL.md'; Text = "slow version $version`n" }) "live: history $version"
        }
        $wantGamma = (Get-UpstreamFiles $c.Repo $head 'skills')['gamma/SKILL.md'].Id
        [void](Wait-Live {
            try { (Get-PublicCommit $c.Repo $c.Branch) -eq $head -and (Get-PublicFiles $c.Repo $c.Branch 'skills')['gamma/SKILL.md'] -eq $wantGamma }
            catch { $false }
        } 'public history and current files' 120 5000)
        $before = @{ mine = Get-BlobId (Join-Path $mine 'SKILL.md'); stray = Get-BlobId (Join-Path $c.Skills 'stray.txt'); beta = Get-BlobId (Join-Path $c.Skills 'beta\SKILL.md'); extra = Get-BlobId (Join-Path $c.Skills 'gamma\my-extra.md') }

        Add-Repo $c
        $historyClock = [Diagnostics.Stopwatch]::StartNew()
        $r = Tss $c @('check')
        Check 'a skill that was not there is installed (alpha)' ((Compare-Skill $c 'alpha').Count -eq 0 -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\SKILL.md')))
        $historyClock.Stop()
        Note "Historical quick slice and initial install: $($historyClock.ElapsedMilliseconds) ms (eight historical candidates at most)."
        Check 'my own skill and my loose file are untouched' ((Get-BlobId (Join-Path $mine 'SKILL.md')) -eq $before.mine -and (Get-BlobId (Join-Path $c.Skills 'stray.txt')) -eq $before.stray)
        Check 'a different folder with an upstream name is not overwritten (beta)' ((Get-BlobId (Join-Path $c.Skills 'beta\SKILL.md')) -eq $before.beta -and $r.Text -match 'beta') ''
        Check 'the check says so (attention)' ($r.Code -eq 1) "exit $($r.Code)"
        Check 'a whole older GitHub copy is adopted and updated explicitly (gamma)' ((Read-Skill $c 'gamma\SKILL.md') -eq $newGamma -and $r.Text -match 'older GitHub copy of gamma')
        Check 'verified old upstream files need no backup' (@(Get-ChildItem (Join-Path $c.Root '.tss-work') -Directory -Filter 'backup-*' -ErrorAction SilentlyContinue).Count -eq 0)
        Check 'an edited old copy is held' ((Read-Skill $c 'older-edited\SKILL.md') -eq "old edited fixture`nmy edit`n")
        Check 'a longer search yields and honestly reports saved work without a tray' ($r.Text -match 'History work is saved' -and (Read-Skill $c 'slow\SKILL.md') -eq "old slow fixture`n")
        $pause = Tss $c @('resolve', $c.Id, 'slow', 'pause')
        Check 'Cancel pauses this search persistently' ($pause.Code -eq 0)
        $paused = Tss $c @('check')
        Check 'a new process leaves the search paused' ($paused.Text -match 'search paused' -and (Read-Skill $c 'slow\SKILL.md') -eq "old slow fixture`n")
        [void](Start-Tray $c)
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -ge 1 } 'the history startup' 60)
        $second = Start-LiveTray $c.Root
        [void]$second.WaitForExit(5000)
        $win = Wait-Live { $w = Find-LiveWindow $c.Tray.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 'the history window' 10
        [void][TssLive]::SetWindowPos($win, [IntPtr](-1), 0, 0, 0, 0, 0x13)
        Start-Sleep -Milliseconds 500
        Check 'the window shows the paused search and its choices' ((Read-Panel $win $UiPanel.Page) -match 'Resume search')
        [void][TssLive]::SendMessageW([TssLive]::GetDlgItem($win, $UiPanel.Page), 0x115, [IntPtr]7, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 300
        [void](Save-Shot $win 'unmanaged-history-paused')
        Test-Layout $win 'paused history'
        Stop-LiveTray $c.Tray
        $c.Tray = $null
        $replace = Tss $c @('resolve', $c.Id, 'slow', 'replace')
        Check 'Use GitHub supersedes the search with a backup' ($replace.Code -eq 0 -and $replace.Text -match 'Backup:' -and (Read-Skill $c 'slow\SKILL.md') -eq "slow version 9`n")
        [void](Tss $c @('check'))
        Check 'no stale search undoes the override' ((Read-Skill $c 'slow\SKILL.md') -eq "slow version 9`n")

        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/gamma/SKILL.md'; Text = "---`nname: gamma`ndescription: gamma once adopted`n---`n" }) 'live: gamma changes')
        $tries = 0
        do { $tries++; [void](Tss $c @('check')); $adopted = (Read-Skill $c 'gamma\SKILL.md') -match 'once adopted'; if (-not $adopted) { Start-Sleep 5 } } while (-not $adopted -and $tries -lt 4)
        Check 'a copy identical in every file GitHub has was taken over, my own file beside it notwithstanding: it follows upstream (gamma)' $adopted "$tries check(s)"
        Check 'the update kept my own file in gamma' ((Get-BlobId (Join-Path $c.Skills 'gamma\my-extra.md')) -eq $before.extra)

        $r = Tss $c @('remove', $c.Id, '--delete-skills')
        Check 'remove --delete-skills deletes what it installed (alpha, and gamma but for my file)' (-not (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha')) -and (Get-Entries (Join-Path $c.Skills 'gamma')) -eq 'my-extra.md') (((Get-SkillNames $c) -join ',') + '; gamma: ' + (Get-Entries (Join-Path $c.Skills 'gamma')))
        Check 'and leaves everything that was mine (mine-only, stray.txt, beta, gamma\my-extra.md)' ((Get-BlobId (Join-Path $mine 'SKILL.md')) -eq $before.mine -and (Get-BlobId (Join-Path $c.Skills 'stray.txt')) -eq $before.stray -and (Get-BlobId (Join-Path $c.Skills 'beta\SKILL.md')) -eq $before.beta -and (Get-BlobId (Join-Path $c.Skills 'gamma\my-extra.md')) -eq $before.extra)
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
        # Added like any repository, without --use-token: the app has to find out by itself that it needs the token.
        $c = New-Env 'private' $PrivateRepo -NoBranch -WithProxy
        Add-Repo $c @('--every', '6h')
        $r = Tss $c @('check')
        $printed.Add($r.Text)
        $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills'
        $have = (Get-SkillNames $c) -join ','
        Check 'without being told, the private repository installs with the token, equal to upstream' ($r.Code -eq 0 -and $diff.Count -eq 0 -and $have -eq 'alpha,beta,gamma') "exit $($r.Code); $have; $($diff.Count) difference(s)"
        $log = @($c.Proxy.Entries())
        $rest = @($log | Select-Object -Skip 1)
        $first = if ($log.Count) { $log[0] } else { '' }
        Check 'the first listing went without the token and was not found; every request after it carried the token' ($first -match 'auth=no' -and $first -match '-> 404 ' -and $rest.Count -gt 0 -and @($rest | Where-Object { $_ -notmatch 'auth=yes' }).Count -eq 0) "$($log.Count) request(s); first: $(($first -split '\|')[0].Trim())"
        $r = Tss $c @('token', 'status')
        $printed.Add($r.Text)
        Check 'the app remembers that the repository needs the token' ($r.Text -match "Repositories that send the token: $([regex]::Escape($c.Id))") "exit $($r.Code)"
        $seen = $log.Count
        $r = Tss $c @('check')
        $printed.Add($r.Text)
        $again = @($c.Proxy.Entries() | Select-Object -Skip $seen)
        Check 'a second check sends the token at once, and GitHub answered "not modified"' ($r.Code -eq 0 -and $again.Count -ge 1 -and @($again | Where-Object { $_ -notmatch 'auth=yes' }).Count -eq 0 -and @($again | Where-Object { $_ -match '-> 304 ' }).Count -ge 1) "exit $($r.Code); $($again.Count) request(s)"

        # No sign-in anywhere: the app cannot find gh, whose token lives in the Windows keyring whatever its settings
        # say. A token saved in Credential Manager is shared by every copy of the app and is never touched, so with one
        # saved this half cannot run.
        $d = New-Env 'private-anon' $PrivateRepo -NoBranch -WithProxy
        $noSignIn = @{ PATH = "$env:SystemRoot\System32;$env:SystemRoot"; GH_TOKEN = $null; GITHUB_TOKEN = $null }
        Add-Repo $d @('--every', '6h')
        $r = Tss $d @('token', 'status') $noSignIn
        $printed.Add($r.Text)
        if ($r.Text -match 'Saved token: yes') {
            Note 'skipped the half without any sign-in: a token is saved in Windows Credential Manager, and it is not touched'
        } else {
            $before = Get-DiskStamp $d.Skills
            $r = Tss $d @('check') $noSignIn
            $printed.Add($r.Text)
            Check 'without any sign-in the repository is not found, and the app says how to sign in' ($r.Code -ne 0 -and $r.Text -match 'could not find' -and $r.Text -match 'gh auth login') "exit $($r.Code)"
            Check 'and nothing was installed' ((Get-SkillNames $d).Count -eq 0 -and (Get-DiskStamp $d.Skills) -eq $before)
            $anon = @($d.Proxy.Entries())
            Check 'and no request carried a token' ($anon.Count -gt 0 -and @($anon | Where-Object { $_ -match 'auth=yes' }).Count -eq 0) "$($anon.Count) request(s)"
        }

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
        $win = Open-Window $c.Tray
        Open-Repo $win 0
        $rows = @(Read-Skills $win)
        $text = ($rows | ForEach-Object { "$($_.Name): $($_.Tag)" }) -join "`n"
        Check 'the window lists the skills with the characters written out' ($rows.Count -ge 2 -and $text -match '\\u\{202e\}' -and -not (Has-Danger $text)) (Show-Safe ($text -replace "`n", ' | '))
        $details = @($rows | ForEach-Object { [string]$_.Detail + [string]$_.Facts })
        $unsafeDetails = @($details | Where-Object { Has-Danger $_ })
        $missing = @($rows | Where-Object { (-not $_.Facts -and -not $_.Detail) -or ($_.Tag -eq 'Edited on this PC' -and (-not $_.Facts -or -not $_.Detail)) } | ForEach-Object { $_.Name })
        Check 'each native row has safe summary text; edited copies have both facts and explanation' ($rows.Count -ge 2 -and $unsafeDetails.Count -eq 0 -and $missing.Count -eq 0) ("$($rows.Count) row(s); $($unsafeDetails.Count) with a control or direction character; missing text: " + (Show-Safe ($missing -join ', ')))
        Note ('what each skill says: ' + (Show-Safe (($rows | ForEach-Object { "$($_.Name): " + (([string]$_.Detail + ' ' + [string]$_.Facts).Trim() -replace "\s+", ' ') }) -join ' || ')))

        # No command is printed for such a name, so this window is the way to settle that skill.
        $rlo = @($rows | Where-Object { $_.Name -like 'rlo-*' })[0]
        $use = @($rlo.Buttons | Where-Object { $_.Text -eq 'Use the GitHub version' })
        Check 'the edited skill with the override character offers "Use the GitHub version"' ($use.Count -eq 1) (($rlo.Buttons | ForEach-Object { $_.Text }) -join ' | ')
        $resolved = Get-MarkCount $c.Report 'job-resolve'
        if ($use.Count -eq 1) {
            Click-Ui $win $UiPanel.Page $use[0].Id
            [void](Wait-Live { (Get-MarkCount $c.Report 'job-resolve') -gt $resolved } 'the replace to be done' 30)
        }
        $now = [IO.File]::ReadAllText((Join-Path (Join-Path $c.Skills $rloName) 'SKILL.md'))
        Check '"Use the GitHub version" on the override-character skill puts the upstream text back' ($now -notmatch 'my edit' -and $now -match 'right-to-left override') ''
        # engine.lock is skipped: the app holds it shut while a job runs, such as the check it makes after start.
        $saved = @(Get-ChildItem -LiteralPath $c.Root -Recurse -File -Force -ErrorAction SilentlyContinue | Where-Object { $_.FullName -notlike "$($c.Skills)\*" -and $_.FullName -notlike "$($c.Root)\bin\*" -and $_.FullName -notlike "$($c.Root)\out\*" -and $_.Name -ne 'engine.lock' } | Select-String -SimpleMatch 'my edit' -List)
        Check 'and my edit was saved in a backup first' ($saved.Count -ge 1) (($saved | ForEach-Object { Show-Safe $_.Path.Substring($c.Root.Length) }) -join ', ')
        Close-Window $c.Tray
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

        # A file the listing names that the old commit does not have: GitHub answers "not found" for it, which is a
        # change under way, never a missing or private repository.
        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/added.md'; Text = "# Added`n`nafter the install`n" }) 'live: alpha gains a file')
        [void](Wait-Live { (Get-PublicFiles $c.Repo $c.Branch 'skills').Contains('alpha/references/added.md') } 'the listing without a token to show the pushed file' 90 15000)
        $r = Tss $c @('check')
        $same = (Get-DiskStamp $c.Skills) -eq $good
        Check 'a listed file the old commit does not have: "Changed while checking", not "not found" or "private", nothing changed' ($r.Code -ne 0 -and $r.Text -match 'Changed while checking' -and $r.Text -notmatch 'not found|private' -and $same) "exit $($r.Code); folder unchanged: $same"

        [void](Push-Upstream $c.Repo $c.Branch @(@{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nchanged after the install`n" }) 'live: alpha changes')
        $r = Tss $c @('check')
        $replayed = @($c.Proxy.Entries() | Where-Object { $_ -match 'REPLAYED AN EARLIER ANSWER' })
        Check 'the proxy really gave an old commit together with the new listing' ($replayed.Count -ge 1) "$($replayed.Count) replayed answer(s)"
        $same = (Get-DiskStamp $c.Skills) -eq $good
        Check 'the app noticed (the download did not match the listing): "Changed while checking", nothing changed, exit not 0' ($r.Code -ne 0 -and $r.Text -match 'Changed while checking' -and $same) "exit $($r.Code); folder unchanged: $same"

        $c.Proxy.Thaw()
        $r = Tss $c @('check')
        $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills'
        Check 'once the old answer is gone, the change arrives and the folder equals upstream' ($r.Code -eq 0 -and $diff.Count -eq 0 -and (Read-Skill $c 'alpha\references\guide.md') -match 'changed after the install' -and (Read-Skill $c 'alpha\references\added.md') -match 'after the install') "exit $($r.Code); $($diff.Count) difference(s)"
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
        $win = Open-Window $c.Tray
        $nav = @(Read-Nav $win)
        # The record says the last listing was empty, so this needs no check of its own.
        Check 'after a restart, before any check, the repository list already says "No skills found"' ($nav.Count -ge 1 -and $nav[0].Sub -eq 'No skills found') (($nav | ForEach-Object { "$($_.Label) | $($_.Sub)" }) -join ' ;; ')
        [void](Invoke-TrayCheck $c.Tray $c.Report)
        Start-Sleep -Milliseconds 800
        $nav = @(Read-Nav $win)
        Check 'after "Sync now", the repository list says "No skills found"' ($nav.Count -ge 1 -and $nav[0].Sub -eq 'No skills found') (($nav | ForEach-Object { "$($_.Label) | $($_.Sub)" }) -join ' ;; ')
        $hero = Read-Hero $win
        Check 'and the top of the window says the repository needs attention' ($hero.Title -eq '1 repository needs your attention') "$($hero.Title) | $($hero.Sub)"
        Open-Repo $win 0
        $lead = Read-Ui $win $UiPanel.Page $UiId.InfoLead
        $detail = Read-Ui $win $UiPanel.Page $UiId.InfoText
        Check 'its page says "No skills found." and tells what to do (the address with the folder)' ($lead -eq 'No skills found.' -and $detail -match 'tree/main/skills') ("$lead " + ($detail -replace "\s+", ' '))
        Check 'with a "Try again" button' ($null -ne (Read-Ui $win $UiPanel.Page $UiId.Retry))
        Close-Window $c.Tray
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
    # A picture taken at once can show controls not drawn yet, or a question that just closed still fading out.
    [void](Wait-Maybe { if (-not [TssLive]::Pending($Window)) { $true } } 3)
    Start-Sleep -Milliseconds 300
    [void][TssLive]::DwmFlush()
    $over = [TssLive]::CoveredBy($Window)
    if ($over -ne '') {
        # A previous window can still be in front just after navigation. Raise only the fixture being captured.
        [void][TssLive]::SetWindowPos($Window, [IntPtr]::Zero, 0, 0, 0, 0, 0x13)
        [void](Wait-Maybe { [TssLive]::CoveredBy($Window) -eq '' } 2)
        $over = [TssLive]::CoveredBy($Window)
    }
    try {
        if ($over -eq '') { [void][TssLive]::CaptureScreen($Window, $path, 0) }
        else { [void][TssLive]::Capture($Window, $path); Note "${Name}: a window ($over) is on top of this one, so its picture was taken with PrintWindow" }
    } catch { Note "no picture for ${Name}: $($_.Exception.Message)"; return $null }
    [pscustomobject]@{ Path = $path; Screen = ($over -eq '') }
}

# What the window has drawn: nothing is left waiting to be painted, and in the picture each of the given controls (pairs
# of a panel and an id) is more than one flat colour. A control that was not drawn would be a single colour; one that
# was has its text, glyph or border in it.
function Test-Painted($Shot, [IntPtr]$Window, [string]$State, [object[]]$Parts) {
    $quiet = Wait-Maybe { if (-not [TssLive]::Pending($Window)) { $true } } 3
    Check "${State}: nothing is left waiting to be painted" ($null -ne $quiet) ('waiting: ' + [TssLive]::Pending($Window))
    if (-not $Shot) { Check "${State}: a picture of the window was taken" $false; return }
    if (-not $Shot.Screen) { Check "${State}: the painted controls can be checked on screen" $false 'obstructed'; return }
    $w = ([TssLive]::Bounds($Window)) -split ','
    $seen = @(foreach ($part in $Parts) {
            $h = Find-Ui $Window $part[0] $part[1]
            if ($h -eq [IntPtr]::Zero) { [pscustomobject]@{ Name = "$($part[0])/$($part[1])"; Contrast = -1 }; continue }
            $b = ([TssLive]::Bounds($h)) -split ','
            $x = [int]$b[0] - [int]$w[0]; $y = [int]$b[1] - [int]$w[1]
            [pscustomobject]@{ Name = "$($part[0])/$($part[1])"; Contrast = [TssLive]::Contrast($Shot.Path, $x, $y, [int]$b[2] - [int]$b[0], [int]$b[3] - [int]$b[1]) }
        })
    $bare = @($seen | Where-Object { $_.Contrast -lt 40 })
    Check "${State}: the picture shows each control looked at drawn" ($bare.Count -eq 0) (($seen | ForEach-Object { "$($_.Name) varies by $($_.Contrast)" }) -join '; ')
}

# Whether a control (a pair of a panel and an id) looks the same in two pictures of the window, taken before and after
# something else in the window painted again. Controls do not clip each other, so one that another covers loses the
# part covered whenever that other one paints again by itself.
function Test-Unchanged($Before, $After, [IntPtr]$Window, [string]$State, [object[]]$Part, [string]$What) {
    if (-not $Before -or -not $After) { Check "${State}: pictures of the window were taken" $false; return }
    if (-not ($Before.Screen -and $After.Screen)) { Check "${State}: $What can be compared on screen" $false 'obstructed'; return }
    $h = Find-Ui $Window $Part[0] $Part[1]
    if ($h -eq [IntPtr]::Zero) { Check "${State}: $What is there" $false "no control $($Part[1]) on panel $($Part[0])"; return }
    $w = ([TssLive]::Bounds($Window)) -split ','
    $b = ([TssLive]::Bounds($h)) -split ','
    $n = [TssLive]::Differ($Before.Path, $After.Path, [int]$b[0] - [int]$w[0], [int]$b[1] - [int]$w[1], [int]$b[2] - [int]$b[0], [int]$b[3] - [int]$b[1])
    Check "${State}: $What looks the same as before" ($n -eq 0) $(if ($n -lt 0) { 'the two pictures are not the same size' } else { "$n pixels of $($Part[0])/$($Part[1]) changed" })
}

# Each part of the window shown is inside it and none is over another; inside each part, every control that can be
# used is inside it from left to right (the pages scroll, so up and down is not checked), and no control, whether Tab
# visits it or not, is over another. Controls are sibling windows that do not clip each other, so one that paints again
# by itself (under the mouse, say) would draw over the part of another that it covers.
function Test-Layout([IntPtr]$Window, [string]$State) {
    $name = { param($b) "$($b.Class)#$($b.Id) at $($b.X),$($b.Y) $($b.W)x$($b.H)" }
    $overlaps = {
        param($boxes)
        $boxes = @($boxes | Where-Object { $_.W -gt 0 -and $_.H -gt 0 })
        for ($i = 0; $i -lt $boxes.Count; $i++) {
            for ($j = $i + 1; $j -lt $boxes.Count; $j++) {
                $a = $boxes[$i]; $b = $boxes[$j]
                if ($a.X -lt $b.X + $b.W -and $b.X -lt $a.X + $a.W -and $a.Y -lt $b.Y + $b.H -and $b.Y -lt $a.Y + $a.H) { (& $name $a) + ' over ' + (& $name $b) }
            }
        }
    }
    $size = ([TssLive]::Client($Window)) -split ','
    $panels = @(Get-Layout $Window | Where-Object { $_.Shown })
    $outside = New-Object System.Collections.Generic.List[string]
    foreach ($p in $panels) {
        if ($p.X -lt 0 -or $p.Y -lt 0 -or $p.X + $p.W -gt [int]$size[0] -or $p.Y + $p.H -gt [int]$size[1]) { $outside.Add('panel ' + (& $name $p)) }
    }
    $over = New-Object System.Collections.Generic.List[string]
    foreach ($o in @(& $overlaps $panels)) { $over.Add('panels: ' + $o) }
    foreach ($p in $panels) {
        $width = [int](([TssLive]::Client($p.Handle)) -split ',')[0]
        $boxes = @(Get-Layout $p.Handle | Where-Object { $_.Shown })
        foreach ($b in @($boxes | Where-Object { $_.Tab })) { if ($b.X -lt 0 -or $b.X + $b.W -gt $width) { $outside.Add("in $($p.Id): " + (& $name $b)) } }
        foreach ($o in @(& $overlaps $boxes)) { $over.Add("in $($p.Id): " + $o) }
    }
    Check "${State}: every part and control is inside the window" ($outside.Count -eq 0) ($outside -join '; ')
    Check "${State}: no control is over another" ($over.Count -eq 0) ($over -join '; ')
}

# Tab must go through each part of the window in reading order, not jump about: each control it visits is no higher
# than the one before (give or take a few pixels), and one beside the one before is to its right.
function Test-TabOrder([IntPtr]$Window, [string]$State) {
    $back = New-Object System.Collections.Generic.List[string]
    $order = New-Object System.Collections.Generic.List[string]
    foreach ($p in @(Get-Layout $Window | Where-Object { $_.Shown })) {
        $tabs = @(Get-Layout $p.Handle | Where-Object { $_.Tab -and $_.Shown -and $_.On })
        $order.Add("$($p.Id): " + (($tabs | ForEach-Object { $_.Id }) -join ' '))
        for ($i = 1; $i -lt $tabs.Count; $i++) {
            $a = $tabs[$i - 1]; $b = $tabs[$i]
            $up = $b.Y + 12 -lt $a.Y
            $left = [math]::Abs($b.Y - $a.Y) -le 12 -and $b.X -lt $a.X
            if ($up -or $left) { $back.Add("in $($p.Id): $($a.Id) then $($b.Id)") }
        }
    }
    Check "${State}: Tab visits the controls in reading order" ($back.Count -eq 0) (($order -join '; ') + $(if ($back.Count) { '; goes back at ' + ($back -join ', ') } else { '' }))
}

# The texts of the controls in a part of a window, joined, to read in a report or match.
function Read-Panel([IntPtr]$Window, [int]$Panel) {
    $p = [TssLive]::GetDlgItem($Window, $Panel)
    if ($p -eq [IntPtr]::Zero) { return '' }
    (@(Get-Layout $p | Where-Object { $_.Shown } | ForEach-Object { [TssLive]::Text($_.Handle) -replace "`n", ' / ' }) | Where-Object { $_ }) -join ' | '
}

function Show-Nav([IntPtr]$Window) { (@(Read-Nav $Window) | ForEach-Object { "$($_.Label) / $($_.Sub)" }) -join ' ;; ' }

function Get-Width([IntPtr]$Handle) { $b = ([TssLive]::Bounds($Handle)) -split ','; [int]$b[2] - [int]$b[0] }

function Move-Pointer([IntPtr]$Handle) {
    $b = ([TssLive]::Bounds($Handle)) -split ','
    [void][TssLive]::SetCursorPos([int](([int]$b[0] + [int]$b[2]) / 2), [int](([int]$b[1] + [int]$b[3]) / 2))
}

# The list on the left widens when the mouse rests on it and a name in it is cut, taking the room from the page, and
# narrows when the mouse leaves it. The window asks Windows where the mouse is, so this moves the real mouse pointer,
# and puts it back afterwards.
function Test-ListHover($c, [IntPtr]$Window, [switch]$Diff) {
    $over = [TssLive]::CoveredBy($Window)
    if ($over -ne '') { Check 'the list hover probe has an unobstructed view' $false $over; return }
    $listPanel = if ($Diff) { $UiPanel.Files } else { $UiPanel.Nav }
    $pagePanel = if ($Diff) { $UiPanel.Code } else { $UiPanel.Page }
    $entry = if ($Diff) { $UiId.File + 4 } else { $UiId.NavRepo }
    $mark = if ($Diff) { 'files' } else { 'nav' }
    $state = if ($Diff) { 'the diff file list widened' } else { 'the repository list widened' }
    $nav = [TssLive]::GetDlgItem($Window, $listPanel)
    $pane = [TssLive]::GetDlgItem($Window, $pagePanel)
    $was = ([TssLive]::CursorAt()) -split ','
    try {
        # The pointer could be resting on the list already.
        Move-Pointer $pane
        Start-Sleep -Milliseconds 300
        if ($Diff) {
            Click-Ui $Window $listPanel $entry
            Start-Sleep -Milliseconds 100
            # Keep both panes away from the top: widening must not lose the place or the selected file.
            [void][TssLive]::SendMessageW($nav, 0x115, [IntPtr]1, [IntPtr]::Zero)
            [void][TssLive]::SendMessageW($pane, 0x115, [IntPtr]1, [IntPtr]::Zero)
        }
        $listScroll = [TssLive]::GetScrollPos($nav, 1)
        $pageScroll = [TssLive]::GetScrollPos($pane, 1)
        $narrow = Get-Width $nav
        $both = $narrow + (Get-Width $pane)
        $windowWidth = Get-Width $Window
        $half = [int](([TssLive]::Client($Window)) -split ',')[0] / 2
        $marks = Get-MarkCount $c.Report $mark
        if ($Diff) { [void](Save-Shot $Window '09a-files-narrow') }
        Move-Pointer (Get-Ui $Window $listPanel $entry)
        Start-Sleep -Milliseconds 100
        $early = Get-Width $nav
        $wide = Wait-Maybe { $n = Get-Width $nav; if ($n -gt $narrow) { $n } } 3 25
        if ($null -ne $wide) { [void](Wait-Maybe { if ((Get-MarkCount $c.Report $mark) -gt $marks) { $true } } 3 25) }
        $took = @(Read-Marks $c.Report | Where-Object { $_.Mark -eq $mark } | Select-Object -Skip $marks | ForEach-Object { $_.TookMs })
        $evidence = "list $narrow, then $early after 0.1 s, then $wide px; list and page $both, then $($wide + (Get-Width $pane)) px; half the window $half px; re-laid out in $($took -join ', ') ms"
        Check "${state}: resting the mouse on a cut name widens the list after a moment, taking room from the page" ($early -eq $narrow -and $null -ne $wide -and $wide -le $half + 1 -and [math]::Abs($wide + (Get-Width $pane) - $both) -le 1 -and (Get-Width $Window) -eq $windowWidth) $evidence
        $pic = Save-Shot $Window $(if ($Diff) { '09b-files-widened' } else { '05b-list-widened' })
        $painted = if ($Diff) { @(@($UiPanel.Files, $entry), @($UiPanel.Head, $UiId.Title)) } else { @(@($UiPanel.Nav, $UiId.NavRepo), @($UiPanel.Page, $UiId.Title)) }
        Test-Painted $pic $Window $state $painted
        Test-Layout $Window $state
        if ($Diff) {
            Check 'widening the diff keeps both scroll positions' ($listScroll -gt 0 -and $pageScroll -gt 0 -and [TssLive]::GetScrollPos($nav, 1) -eq $listScroll -and [TssLive]::GetScrollPos($pane, 1) -eq $pageScroll) "files $listScroll, code $pageScroll"
            Move-Pointer (Get-Ui $Window $listPanel ($entry + 1))
            Start-Sleep -Milliseconds 300
            Check 'moving between diff files keeps the list wide' ((Get-Width $nav) -eq $wide)
        }
        Move-Pointer $pane
        # The window sizes the list, then the page: wait for both, as a reader from outside can see one between them.
        $back = Wait-Maybe { if ((Get-Width $nav) -eq $narrow -and (Get-Width $nav) + (Get-Width $pane) -eq $both) { $true } } 3 25
        Check "${state}: moving the mouse onto the page narrows the list again" ($null -ne $back -and (Get-Width $nav) + (Get-Width $pane) -eq $both) ("list $(Get-Width $nav) px, page $(Get-Width $pane) px")
        if ($Diff) {
            Check 'narrowing the diff keeps both scroll positions' ([TssLive]::GetScrollPos($nav, 1) -eq $listScroll -and [TssLive]::GetScrollPos($pane, 1) -eq $pageScroll)
            Move-Pointer (Get-Ui $Window $listPanel $entry)
            Start-Sleep -Milliseconds 100
            Move-Pointer $pane
            Start-Sleep -Milliseconds 300
            Check 'passing over the diff file list does not widen it' ((Get-Width $nav) -eq $narrow)
            Move-Pointer (Get-Ui $Window $listPanel $entry)
            $wideAgain = Wait-Maybe { if ((Get-Width $nav) -gt $narrow) { $true } } 3 25
            Send-Key (Get-Ui $Window $UiPanel.Head $UiId.Back) $VK_ESCAPE
            $left = Wait-Maybe { if (-not [TssLive]::Shown($nav)) { $true } } 5
            Move-Pointer ([TssLive]::GetDlgItem($Window, $UiPanel.Page))
            $dpi = [int][TssLive]::GetDpiForWindow($Window)
            Check 'leaving the widened diff resets the repository list width' ($wideAgain -and $left -and [math]::Abs((Get-Width ([TssLive]::GetDlgItem($Window, $UiPanel.Nav))) - 248 * $dpi / 96) -le 1)
        }
    } finally {
        [void][TssLive]::SetCursorPos([int]$was[0], [int]$was[1])
    }
}

# Send pointer input to the row itself; BM_CLICK would bypass its hit testing.
function Click-RowAt([IntPtr]$Row, [int]$X) {
    $size = [TssLive]::Client($Row) -split ','
    $lp = [IntPtr](([int]([int]$size[1] / 2) -shl 16) -bor $X)
    [void][TssLive]::PostMessageW($Row, $WM_LBUTTONDOWN, [IntPtr]1, $lp)
    [void][TssLive]::PostMessageW($Row, $WM_LBUTTONUP, [IntPtr]::Zero, $lp)
}

# Send real chords only on the dedicated UI desktop, and only while this fixture owns keyboard focus.
function Send-FocusedKeys([IntPtr]$Window, [IntPtr]$Control, [string]$Keys) {
    if ($Control -eq [IntPtr]::Zero -or [TssLive]::GetForegroundWindow() -ne $Window -or [TssLive]::Focused($Window) -ne $Control) {
        throw "Cannot send $Keys safely: the fixture control does not have foreground keyboard focus."
    }
    [System.Windows.Forms.SendKeys]::SendWait($Keys)
}

function Choose-RowAction($c, [IntPtr]$Window, [int]$Id, [int]$Key, [string]$Action) {
    $row = Get-Ui $Window $UiPanel.Page $Id
    Click-RowAt $row 5 # focus the flat row before using the keyboard
    Send-Key $row $Key
    $menu = Wait-Menu $c.Tray
    if ($menu -eq [IntPtr]::Zero) { throw "Row key $Key did not open the actions menu for $Action" }
    $items = @([TssLive]::MenuItems($menu) | ForEach-Object { ($_ -replace '&', '') -replace ' \(default\)$', '' })
    $common = @('Open folder', 'Preview', 'Copy path', 'Copy GitHub link', 'Open on GitHub')
    Check 'the row menu labels every available everyday action' (@($common | Where-Object { $_ -notin $items }).Count -eq 0) ($items -join ' | ')
    $index = [array]::IndexOf($items, $Action)
    if ($index -lt 0) { Send-Key $menu $VK_ESCAPE; throw "Missing row menu item: $Action" }
    Choose-MenuItem $menu $index
    [void](Wait-Live { (Find-LiveWindow $c.Tray.Id $MENU_CLASS -Visible) -eq [IntPtr]::Zero } 'the row menu to close' 5)
}

function Test-BodyTip($c, $Row) {
    $parts = @($Row.Facts, $Row.Detail | Where-Object { $_ } | ForEach-Object { $_ -replace "`r`n", "`n" })
    $tip = Wait-Maybe {
        $t = Find-LiveWindow $c.Tray.Id 'tooltips_class32' -Visible
        if ($t -eq [IntPtr]::Zero) { return }
        $label = [TssLive]::Text($t, 256 + 2 * ($parts -join '').Length) -replace "`r`n", "`n"
        if ($parts.Count -and @($parts | Where-Object { -not $label.Contains($_) }).Count -eq 0) { $label }
    } 5 50
    Check "hovering over the body of $($Row.Name) shows its facts and explanation when present" ($null -ne $tip) ($parts -join ' / ')
}

# Read the flat row before interacting; exercise both its keyboard menu and its painted pointer targets.
function Test-SkillRow($c, [IntPtr]$Window) {
    $dot = [string][char]0x00B7
    # A file of my own in beta: it is mine, so it changes nothing and Preview does not show it.
    $mine = Join-Path $c.Skills 'beta\my-beta-notes.md'
    [IO.File]::WriteAllText($mine, "my notes`n")
    Wait-Settled $c
    try {
        $row = @(Read-Skills $Window | Where-Object { $_.Name -eq 'beta' })
        if ($row.Count -ne 1) { Check 'beta is listed' $false; return }
        $id = $row[0].Id
        $beta = $row[0]
        $files = @((Get-UpstreamFiles $c.Repo $c.Ref 'skills').Keys | Where-Object { $_ -like 'beta/*' } | ForEach-Object { $_.Substring(5) } | Sort-Object)
        $commit = ([string](Invoke-Gh @('api', "repos/$($c.Repo)/git/ref/heads/$($c.Branch)", '--jq', '.object.sha'))).Trim().ToLowerInvariant()
        $count = if ($files.Count -eq 1) { '1 file' } else { "$($files.Count) files" }
        $facts = if ($beta) { [string]$beta.Facts } else { '' }
        Check 'the installed row includes its version, file count, size and update time' ($facts -match "^Version $($commit.Substring(0, 7)) $dot $count, \d+(\.\d)? (bytes?|KB|MB) $dot Updated .+") "$facts (GitHub: $($commit.Substring(0, 7)), $count)"
        Check 'the installed row has no separate action buttons' ($beta.Buttons.Count -eq 0)
        $pane = [TssLive]::GetDlgItem($Window, $UiPanel.Page)
        [void][TssLive]::SendMessageW($pane, 0x115, [IntPtr]7, [IntPtr]::Zero) # SB_BOTTOM
        $pic = Save-Shot $Window '05c-flat-skill'
        Test-Painted $pic $Window 'a flat installed skill' (,@($UiPanel.Page, $id))

        # ---- Copy path and Copy GitHub link
        $clipWas = Get-Clipboard -Raw -ErrorAction SilentlyContinue
        try {
            $path = Join-Path $c.Skills 'beta'
            Choose-RowAction $c $Window $id $VK_RETURN 'Copy path'
            $copied = Wait-Maybe { if ((Get-Clipboard -Raw) -eq $path) { $true } } 5
            $said = Wait-Maybe { $s = (Read-Hero $Window).Sub; if ($s -like 'Copied: *') { $s } } 5
            Check 'Copy path puts the folder of the skill on the clipboard, and says so' ($copied -and $said -eq "Copied: $path") "clipboard: $(Get-Clipboard -Raw) / $((Read-Hero $Window).Sub)"
            $link = "https://github.com/$($c.Repo)/tree/$($c.Ref)/skills/beta"
            Choose-RowAction $c $Window $id $VK_APPS 'Copy GitHub link'
            $copied = Wait-Maybe { if ((Get-Clipboard -Raw) -eq $link) { $true } } 5
            Check 'Copy GitHub link puts the address of the skill''s folder on GitHub on the clipboard' ($null -ne $copied) "clipboard: $(Get-Clipboard -Raw)"
        } finally {
            if ($clipWas) { Set-Clipboard -Value $clipWas } else { [void][TssLive]::ClearClipboard() }
        }

        # ---- Preview, and back
        Choose-RowAction $c $Window $id $VK_RETURN 'Preview'
        [void](Wait-Maybe { Read-Ui $Window $UiPanel.Files $UiId.File } 15)
        $entries = @(for ($i = 0; $i -lt 500; $i++) { $t = Read-Ui $Window $UiPanel.Files ($UiId.File + $i); if ($null -eq $t) { break }; $t })
        $names = @($entries | ForEach-Object { ($_ -split "`n", 2)[0] } | Sort-Object)
        $odd = @($entries | Where-Object { ($_ -split "`n", 2)[1] -notmatch '^(1 line|\d+ lines|Empty|Not text)$' })
        Check 'Preview lists the files GitHub put in the skill, each with how many lines it has, and not my own file' ($entries.Count -gt 0 -and ($names -join '|') -eq ($files -join '|') -and $odd.Count -eq 0) (($entries -join ' / ') -replace "`n", ' - ')
        $head = Read-Panel $Window $UiPanel.Head
        Check 'it names the skill, says it shows the files as they are on this PC without mine, and offers no choice' ((Read-Ui $Window $UiPanel.Head $UiId.Title) -eq 'beta' -and $head -match 'Files you added are not shown' -and $head -notmatch 'Use the GitHub version|Keep my version') $head
        $pic = Save-Shot $Window '05d-preview'
        Test-Painted $pic $Window 'a Preview' @(@($UiPanel.Head, $UiId.Back), @($UiPanel.Head, $UiId.Title), @($UiPanel.Files, $UiId.File))
        if ($pic -and $pic.Screen) {
            # The lines are drawn by the code panel itself, not by controls in it.
            $wb = ([TssLive]::Bounds($Window)) -split ','
            $cb = ([TssLive]::Bounds([TssLive]::GetDlgItem($Window, $UiPanel.Code))) -split ','
            $contrast = [TssLive]::Contrast($pic.Path, [int]$cb[0] - [int]$wb[0], [int]$cb[1] - [int]$wb[1], [int]$cb[2] - [int]$cb[0], [int]$cb[3] - [int]$cb[1])
            Check 'a Preview: the picture shows the lines of the first file drawn' ($contrast -ge 40) "the code panel varies by $contrast"
        }
        Test-Layout $Window 'a Preview'
        Send-Key (Get-Ui $Window $UiPanel.Head $UiId.Back) $VK_ESCAPE
        $back = Wait-Maybe { if ([TssLive]::Shown([TssLive]::GetDlgItem($Window, $UiPanel.Page)) -and -not [TssLive]::Shown([TssLive]::GetDlgItem($Window, $UiPanel.Head))) { $true } } 5
        $differs = @(Compare-Skill $c 'beta')
        Check 'Esc goes back to the page, and Preview changed nothing: my file is still there, the rest as on GitHub' ($back -and ($differs -join ', ') -eq 'unexpected: beta/my-beta-notes.md') ($differs -join ', ')

        # ---- hit the controls the row actually painted, rather than calculating their positions from the status pill
        $over = [TssLive]::CoveredBy($Window)
        if ($over -ne '') { Check 'the row probes have an unobstructed view' $false $over; return }
        $was = ([TssLive]::CursorAt()) -split ','
        $opened = $null
        $dpi = [int][TssLive]::GetDpiForWindow($Window)
        $px = { param([int]$Dips) [int][math]::Round($Dips * $dpi / 96) }
        $original = [int[]]([TssLive]::Bounds($Window) -split ',')
        $width = $original[2] - $original[0]; $height = $original[3] - $original[1]
        $probe = {
            param($mode)
            $wb = ([TssLive]::Bounds($Window)) -split ','
            Move-Pointer ([TssLive]::GetDlgItem($Window, $UiPanel.Hero))
            # Preview returned focus to beta. Give focus to a different flat row for the unadorned picture.
            Click-RowAt (Get-Ui $Window $UiPanel.Page $UiId.Skill) 5
            Start-Sleep -Milliseconds 300
            $pane = [TssLive]::GetDlgItem($Window, $UiPanel.Page)
            [void][TssLive]::SendMessageW($pane, 0x115, [IntPtr]7, [IntPtr]::Zero) # SB_BOTTOM: beta and gamma into view
            $rowH = Get-Ui $Window $UiPanel.Page $id
            $away = Save-Shot $Window "05e-$mode-row-mouse-away"
            $at = [TssLive]::Bounds($rowH)
            $rb = $at -split ','
            $cy = [int](([int]$rb[1] + [int]$rb[3]) / 2)
            $w = [int]$rb[2] - [int]$rb[0]; $h = [int]$rb[3] - [int]$rb[1]
            $x = [int]$rb[0] - [int]$wb[0]; $y = [int]$rb[1] - [int]$wb[1]
            [void][TssLive]::SetCursorPos([int]$rb[0] + (& $px 48), $cy) # over beta's name, before its action icons
            $onRow = Save-Shot $Window "05f-$mode-row-hovered"
            Test-BodyTip $c (Read-Skills $Window | Where-Object Id -eq $id)
            $icons = @(if ($away.Screen -and $onRow.Screen -and [TssLive]::Bounds($rowH) -eq $at) {
                [TssLive]::NewGlyphs($away.Path, $onRow.Path, $x, $y, $w, $h, (& $px 6))
            })
            $counts = if ($mode -eq 'compact') { 2..4 } else { 5 }
            Check "${mode}: hover reveals the expected targets after the name" ($icons.Count -in $counts -and $icons[0] -lt ($w / 2)) "expected $($counts -join '/'); new glyph centres: $($icons -join ', ') in $w px"
            if ($icons.Count -notin $counts) { return }

            $tips = @("Open this skill$([char]0x2019)s folder in File Explorer.", 'Preview', 'Copy path', 'Copy GitHub link', 'Open on GitHub')
            for ($i = 0; $i -lt $icons.Count; $i++) {
                $expected = if ($i -eq $icons.Count - 1 -and $icons.Count -lt 5) { 'More actions' } else { $tips[$i] }
                [void][TssLive]::SetCursorPos([int]$rb[0] + $icons[$i], $cy)
                $tip = Wait-Maybe {
                    $t = Find-LiveWindow $c.Tray.Id 'tooltips_class32' -Visible
                    if ($t -ne [IntPtr]::Zero -and ($label = [TssLive]::Text($t)) -ceq $expected) { $label }
                } 5 50
                Check "inline target $($i + 1) is labeled $expected on hover" ($null -ne $tip) $tip
                if (-not $tip) { return } # never activate an unidentified painted target
            }

            $next = Get-Ui $Window $UiPanel.Page ($id + $UiId.SkillStride)
            $nextAt = [TssLive]::Bounds($next)
            Click-RowAt $rowH (& $px 8)
            Move-Pointer ([TssLive]::GetDlgItem($Window, $UiPanel.Hero))
            [void](Wait-Maybe { [TssLive]::Focused($Window) -eq $rowH } 2)
            $menu = Find-LiveWindow $c.Tray.Id $MENU_CLASS -Visible
            $flat = @(Read-Skills $Window | Where-Object Name -eq 'beta')
            Check 'clicking the flat body neither opens a menu nor expands the row' ($menu -eq [IntPtr]::Zero -and $flat.Count -eq 1 -and $flat[0].Buttons.Count -eq 0 -and $flat[0].Facts -and [TssLive]::Bounds($next) -eq $nextAt)
            if ($menu -ne [IntPtr]::Zero) { Send-Key $menu $VK_ESCAPE; return }
            $alpha = Get-Ui $Window $UiPanel.Page $UiId.Skill
            Click-RowAt $alpha 5
            [void](Wait-Live { [TssLive]::Focused($Window) -eq $alpha } 'alpha to have keyboard focus' 3)
            Send-FocusedKeys $Window $alpha '{TAB}'
            $tabbed = Wait-Maybe { [TssLive]::Focused($Window) -eq $rowH } 3
            Check 'Tab moves keyboard focus from alpha to beta' $tabbed
            if (-not $tabbed) { return }
            $focus = Save-Shot $Window "05g-$mode-flat-row-tab-focused"
            $focused = @(if ($focus.Screen -and [TssLive]::Bounds($rowH) -eq $at) {
                [TssLive]::NewGlyphs($away.Path, $focus.Path, $x, $y, $w, $h, (& $px 6))
            })
            Check 'focus keeps the same actions visible when the pointer leaves' ($focused.Count -ge $icons.Count -and ($focused[0..($icons.Count - 1)] -join ',') -eq ($icons -join ','))
            if ($mode -eq 'wide') {
                Send-FocusedKeys $Window $rowH '+{F10}'
                $shortcut = Wait-Menu $c.Tray
                Check 'Shift+F10 on the Tab-focused row opens its actions' ($shortcut -ne [IntPtr]::Zero)
                if ($shortcut -ne [IntPtr]::Zero) {
                    $labels = [TssLive]::MenuItems($shortcut) -join ' | '
                    Check 'the keyboard menu belongs to the skill' ($labels -match 'Copy path' -and $labels -match 'Copy GitHub link') $labels
                    Send-Key $shortcut $VK_ESCAPE
                    [void](Wait-Live { (Find-LiveWindow $c.Tray.Id $MENU_CLASS -Visible) -eq [IntPtr]::Zero } 'the keyboard menu to close' 5)
                }
            }
            if ($icons.Count -lt 5) {
                Click-RowAt $rowH $icons[-1]
                $more = Wait-Menu $c.Tray
                Check 'on a narrow row the last painted target opens More' ($more -ne [IntPtr]::Zero)
                if ($more -ne [IntPtr]::Zero) {
                    $labels = [TssLive]::MenuItems($more) -join ' | '
                    Check 'More includes the hidden GitHub action' ($labels -match 'Open on GitHub') $labels
                    Send-Key $more $VK_ESCAPE
                    [void](Wait-Live { (Find-LiveWindow $c.Tray.Id $MENU_CLASS -Visible) -eq [IntPtr]::Zero } 'More to close' 5)
                }
            }
            [pscustomobject]@{ Handle = $rowH; Icons = $icons }
        }
        try {
            [void][TssLive]::SetForegroundWindow($Window)
            $frame = $width - [int](([TssLive]::Client($Window) -split ',')[0])
            # The supported minimum client width, at this window's DPI. Keep the height and desktop position.
            [void][TssLive]::SetWindowPos($Window, [IntPtr]::Zero, 0, 0, ((& $px 640) + $frame), $height, 0x16)
            [void](& $probe 'compact')
            $area = [System.Windows.Forms.Screen]::FromHandle($Window).WorkingArea
            $wide = [math]::Min([math]::Max($width, ((& $px 980) + $frame)), $area.Width)
            $left = [math]::Max($area.Left, [math]::Min($original[0], $area.Right - $wide))
            [void][TssLive]::SetWindowPos($Window, [IntPtr]::Zero, $left, $original[1], $wide, $height, 0x14)
            $strip = & $probe 'wide'
            if (-not $strip) { return }
            Click-RowAt $strip.Handle $strip.Icons[0]
            $target = [TssLive]::LongPath($path)
            $shell = New-Object -ComObject Shell.Application
            $opened = Wait-Maybe { foreach ($w in @($shell.Windows())) { try { if ([TssLive]::LongPath($w.Document.Folder.Self.Path) -eq $target) { $w } } catch { } } } 10 300
            Check 'Open folder opens the installed skill directory in File Explorer' ($null -ne $opened) $target
        } finally {
            if ($opened) { foreach ($w in @($opened)) { try { $w.Quit() } catch { } } }
            [void][TssLive]::SetWindowPos($Window, [IntPtr]::Zero, $original[0], $original[1], $width, $height, 0x14)
            [void][TssLive]::SetCursorPos([int]$was[0], [int]$was[1])
        }
    } finally {
        Remove-Item -LiteralPath $mine -Force -ErrorAction SilentlyContinue
        Wait-Settled $c
    }
}

function Test-Ui {
    Add-Type -AssemblyName System.Windows.Forms
    $c = New-Env 'ui' -WithProxy
    $keys = Get-StartupKeys $c.Home
    $uiEntry = "`"$([TssLive]::LongPath((Join-Path $c.Root 'bin\tiny-skill-sync.exe')))`" --background"
    try {
        $area = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
        $tray = $null
        $dot = [string][char]0x00B7

        # ---- the first start: nothing is set up, so the window opens by itself, on the welcome page
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $tray = Start-Tray $c
        $win = Wait-Maybe { $w = Find-LiveWindow $tray.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 10 25
        $opened = $clock.ElapsedMilliseconds
        Check 'with nothing set up, the window opens by itself' ($null -ne $win) "visible $opened ms after the start"
        if ($null -eq $win) { return }
        # Keep this throwaway window visible even when the installed app is open too.
        [void][TssLive]::SetWindowPos($win, [IntPtr](-1), 0, 0, 0, 0, 0x13)
        Start-Sleep -Milliseconds 800
        $b = ([TssLive]::Bounds($win)) -split ','
        Check 'the window is on the screen' ([int]$b[0] -ge $area.Left - 16 -and [int]$b[1] -ge $area.Top -and [int]$b[2] -le $area.Right + 16 -and [int]$b[3] -le $area.Bottom + 16) ("window $($b -join ',') in the work area $($area.Left),$($area.Top),$($area.Right),$($area.Bottom)")
        $ping = [TssLive]::Ping($win, 2000)
        Check 'the window answers at once' ($ping -ge 0 -and $ping -lt 200) "$ping ms"
        $hero = Read-Hero $win
        Check 'the top of the window says what to do' ($hero.Title -eq 'Add a repository to start') "$($hero.Title) / $($hero.Sub)"
        Check 'the list of repositories is empty, and the page offers Add repository' (@(Read-Nav $win).Count -eq 0 -and $null -ne (Read-Ui $win $UiPanel.Page $UiId.WelcomeAdd)) (Read-Panel $win $UiPanel.Page)
        Check 'Sync now is greyed out, as there is nothing to sync' (-not [TssLive]::IsWindowEnabled((Get-Ui $win $UiPanel.Hero $UiId.Sync)))
        $pic = Save-Shot $win '01-first-start'
        Test-Painted $pic $win 'first start' @(@($UiPanel.Hero, $UiId.HeroTitle), @($UiPanel.Nav, $UiId.NavAdd), @($UiPanel.Nav, $UiId.NavSettings), @($UiPanel.Page, $UiId.WelcomeAdd))
        Test-Layout $win 'first start'
        Test-TabOrder $win 'first start'

        # ---- Settings
        Click-Ui $win $UiPanel.Nav $UiId.NavSettings
        $folder = Wait-Maybe { Read-Ui $win $UiPanel.Page $UiId.SkillsDir } 5
        Check 'Settings shows the skills folder the app was given' ($folder -eq $c.Skills) $folder
        $default = Read-Ui $win $UiPanel.Page $UiId.DefaultEvery
        Check 'the default check interval is 1 day' ($default -eq '1 day') $default
        # The saved token is shared by every copy of the app, so either way is right; what is shown must agree with it.
        $token = Read-Ui $win $UiPanel.Page $UiId.TokenText
        $save = Read-Ui $win $UiPanel.Page 323
        $delete = Read-Ui $win $UiPanel.Page 324
        $agrees = ($token -match 'No token is saved, so your GitHub CLI sign-in is used' -and $save -like 'Save a token*' -and $null -eq $delete) -or ($token -match 'Your token is saved' -and $save -like 'Replace token*' -and $delete -eq 'Delete token')
        Check 'it says the token is used by itself, and offers to save, replace or delete one as fits' $agrees "$token / $save / $delete"
        $switch = Find-Ui $win $UiPanel.Page $UiId.Startup
        $startNote = Read-Ui $win $UiPanel.Page $UiId.StartupText
        $offSwitch = $switch -ne [IntPtr]::Zero -and [TssLive]::IsWindowEnabled($switch) -and -not (Get-Checked $win $UiPanel.Page $UiId.Startup)
        Check 'it has a Start with Windows switch, off and ready to turn on, and says what it does' ($offSwitch -and $startNote -eq 'Starts in the tray when you sign in to Windows, without opening its window.') $startNote
        $pic = Save-Shot $win '02-settings'
        Test-Painted $pic $win 'Settings' @(@($UiPanel.Page, $UiId.DefaultEvery), @($UiPanel.Page, $UiId.Startup), @($UiPanel.Page, $UiId.SkillsDir), @($UiPanel.Page, 323))
        Test-Layout $win 'Settings'
        Test-TabOrder $win 'Settings'

        # ---- add a repository through its question, the answers slowed down so that the check can be watched
        Click-Ui $win $UiPanel.Nav $UiId.NavAdd
        $ask = Wait-Maybe { $d = Find-Ask $tray; if ($d -ne [IntPtr]::Zero) { $d } } 8
        Check 'Add repository asks for the repository' ($null -ne $ask)
        if ($null -eq $ask) { return }
        Start-Sleep -Milliseconds 400
        $said = Read-Panel $ask $AskPanel
        Note "the question: $said"
        $note = Read-Ui $ask $AskPanel $AskId.Note
        Check 'it has no box for a token; a note says private repositories work by themselves' ($note -like 'Private repositories work automatically*' -and $said -notmatch 'Use my token|Private repository$') $note
        Check 'it offers how often to check, starting at the default' ((Read-Ui $ask $AskPanel $AskId.Combo) -eq 'Default (1 day)') (Read-Ui $ask $AskPanel $AskId.Combo)
        Set-UiText (Get-Ui $ask $AskPanel $AskId.Edit) (Get-Address $c)
        $hint = Wait-Maybe { $t = Read-Ui $ask $AskPanel $AskId.Hint; if ($t -match [regex]::Escape($c.Repo) -and $t -match 'skills folder on') { $t } } 5
        Check 'as the address is typed, the question says what it reads it as' ($null -ne $hint) (Read-Ui $ask $AskPanel $AskId.Hint)
        $pic = Save-Shot $ask '03-add'
        Test-Painted $pic $ask 'the Add question' @(@($AskPanel, $AskId.Heading), @($AskPanel, $AskId.Edit), @($AskPanel, $AskId.Ok), @($AskPanel, $AskId.Cancel))
        Test-Layout $ask 'the Add question'
        Test-TabOrder $ask 'the Add question'
        $c.Proxy.SetMode('api', 'slow:2500')
        Click-Ui $ask $AskPanel $AskId.Ok
        $seen = New-Object System.Collections.Generic.List[string]
        $sample = { $t = Read-Ui $win $UiPanel.Hero $UiId.HeroSub; if ($t -and -not $seen.Contains($t)) { $seen.Add($t) } }

        # ---- the first repository added: the offer to start with Windows comes over the window while the check goes on
        $offer = Wait-Maybe { & $sample; $d = Find-Ask $tray; if ($d -ne [IntPtr]::Zero -and (Read-Ui $d $AskPanel $AskId.Heading) -eq 'Start with Windows?') { $d } } 10
        Check 'adding the first repository offers to start with Windows' ($null -ne $offer)
        if ($null -ne $offer) {
            $said = Read-Ui $offer $AskPanel $AskId.Text
            $yes = Read-Ui $offer $AskPanel $AskId.Ok
            $no = Read-Ui $offer $AskPanel $AskId.Cancel
            $plainly = $said -match 'start in the tray when you sign in' -and $said -match 'does not slow your sign-in down' -and $said -match 'change this in Settings'
            Check 'it says it starts in the tray, does not slow sign-in down and can be changed in Settings, with Turn on and Not now' ($plainly -and $yes -eq 'Turn on' -and $no -eq 'Not now') "$said / $yes / $no"
            $pic = Save-Shot $offer '04a-start-with-windows'
            Test-Painted $pic $offer 'the offer to start with Windows' @(@($AskPanel, $AskId.Heading), @($AskPanel, $AskId.Text), @($AskPanel, $AskId.Ok), @($AskPanel, $AskId.Cancel))
            Test-Layout $offer 'the offer to start with Windows'
            Test-TabOrder $offer 'the offer to start with Windows'
            Click-Ui $offer $AskPanel $AskId.Cancel
            $closed = Wait-Maybe { & $sample; if ((Find-Ask $tray) -eq [IntPtr]::Zero) { $true } } 5
            Check '"Not now" closes it and writes no entry' ($closed -and $null -eq (Read-StartupEntry $keys)) "entry: $(Read-StartupEntry $keys)"
        }
        $worst = 0
        $watch = [Diagnostics.Stopwatch]::StartNew()
        $done = $false
        $pic = $null
        while ($watch.Elapsed.TotalSeconds -lt 40 -and -not $done) {
            $t = Read-Ui $win $UiPanel.Hero $UiId.HeroSub
            if ($t -and -not $seen.Contains($t)) { $seen.Add($t) }
            if ($t -match 'Checking for updates' -and -not $pic) { $pic = Save-Shot $win '04-checking' }
            $answer = [TssLive]::Ping($win, 3000)
            $worst = [math]::Max($worst, $(if ($answer -lt 0) { 3000 } else { $answer }))
            $nav = @(Read-Nav $win)
            if ($nav.Count -ge 1 -and $nav[0].Sub -match 'up to date$') { $done = $true }
            Start-Sleep -Milliseconds 100
        }
        $c.Proxy.SetMode('api', 'pass')
        Check 'the question closed once Add was clicked' ((Find-Ask $tray) -eq [IntPtr]::Zero)
        Check 'while it checked, the top of the window said "Checking for updates"' (@($seen | Where-Object { $_ -match 'Checking for updates' }).Count -ge 1) ($seen -join ' / ')
        Check 'the window kept answering while it checked' ($worst -lt 500) "slowest answer $worst ms over $([int]$watch.Elapsed.TotalSeconds) s"
        Check 'the check ends with the list saying "3 skills, up to date"' ($done -and @(Read-Nav $win)[0].Sub -eq "3 skills $dot up to date") (Show-Nav $win)
        $title = Read-Ui $win $UiPanel.Page $UiId.Title
        $sub = Read-Ui $win $UiPanel.Page $UiId.Sub
        Note "the page: $title / $sub"
        Check 'the page of the new repository is shown: its name, public, its folder and branch, when it was checked' ($title -eq $c.Repo -and $sub -match '^Public' -and $sub -match "the skills folder on $([regex]::Escape($c.Ref))" -and $sub -match 'checked') "$title / $sub"
        $skills = @(Read-Skills $win)
        Note ('the skills: ' + (($skills | ForEach-Object { "$($_.Name) / $($_.Tag)" }) -join ' ;; '))
        Check 'alpha, beta and gamma each show installed status and facts without repair buttons' ((($skills | ForEach-Object { $_.Name }) -join ',') -eq 'alpha,beta,gamma' -and @($skills | Where-Object { $_.Tag -notmatch '^(Installed|Up to date)$' -or -not $_.Facts -or $_.Buttons.Count }).Count -eq 0)
        Check 'the skills are on disk, equal to upstream' ((Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills').Count -eq 0)
        Check 'Check every shows the default, and Keep in sync is on' ((Read-Ui $win $UiPanel.Page $UiId.Every) -eq 'Default (1 day)' -and (Get-Checked $win $UiPanel.Page $UiId.Enabled)) (Read-Ui $win $UiPanel.Page $UiId.Every)
        $hero = Wait-Maybe { $h = Read-Hero $win; if ($h.Title -eq 'All skills are up to date') { $h } } 8
        Check 'the top of the window says all skills are up to date, and how the check the user asked for ended' ($null -ne $hero -and $hero.Sub -like 'Checked: skills were updated*') $(if ($hero) { "$($hero.Title) / $($hero.Sub)" } else { (Read-Hero $win).Title })
        $pic = Save-Shot $win '05-installed'
        Test-Painted $pic $win 'a repository added' @(@($UiPanel.Hero, $UiId.HeroTitle), @($UiPanel.Hero, $UiId.Sync), @($UiPanel.Nav, $UiId.NavRepo), @($UiPanel.Nav, $UiId.NavSettings), @($UiPanel.Page, $UiId.Title), @($UiPanel.Page, $UiId.Every), @($UiPanel.Page, $UiId.Skill))
        Test-Layout $win 'a repository added'
        Test-TabOrder $win 'a repository added'
        Test-ListHover $c $win
        Test-SkillRow $c $win
        Click-Ui $win $UiPanel.Nav $UiId.NavActivity
        $notes = Wait-Maybe { $t = Read-Ui $win $UiPanel.Page $UiId.Note; if ($t) { $t } } 5
        Check 'Activity tells what was done' ($notes -match 'alpha' -or $notes -match 'nstalled') (Read-Panel $win $UiPanel.Page)
        [void](Save-Shot $win '06-activity')

        # ---- Check every, of the repository and the default
        Open-Repo $win 0
        Choose-Ui $win $UiPanel.Page $UiId.Every '12 hours'
        $cfg = Wait-Maybe { $x = Get-Config $c; if ($x -and $x.repos[0].interval -eq '12h') { $x } } 8
        Check 'choosing 12 hours in "Check every" is saved for that repository, and shown' ($null -ne $cfg -and (Read-Ui $win $UiPanel.Page $UiId.Every) -eq '12 hours') (Read-Ui $win $UiPanel.Page $UiId.Every)
        $hero = Wait-Maybe { $h = Read-Hero $win; if ($h.Sub -like "3 skills from 1 repository $dot checked * $dot next check in *") { $h } } 5
        Check 'once the user does something else, the top of the window sums up: the skills, when they were checked, the next check' ($null -ne $hero) ((Read-Hero $win).Sub)
        Click-Ui $win $UiPanel.Nav $UiId.NavSettings
        [void](Wait-Maybe { Read-Ui $win $UiPanel.Page $UiId.DefaultEvery } 5)
        Choose-Ui $win $UiPanel.Page $UiId.DefaultEvery '6 hours'
        $cfg = Wait-Maybe { $x = Get-Config $c; if ($x -and $x.default_interval -eq '6h') { $x } } 8
        Check 'choosing 6 hours as the default in Settings is saved, and the repository keeps its own' ($null -ne $cfg -and $cfg.repos[0].interval -eq '12h') ('default ' + $(if ($cfg) { $cfg.default_interval } else { '-' }))

        # ---- Start with Windows, in Settings
        Click-Ui $win $UiPanel.Page $UiId.Startup
        $on = Wait-Maybe { if ((Read-StartupEntry $keys) -and (Get-Checked $win $UiPanel.Page $UiId.Startup)) { $true } } 8
        $now = Read-StartupEntry $keys
        Check 'turning Start with Windows on writes an entry that starts this copy with --background' ($on -and $now -eq $uiEntry) $now
        [void](Save-Shot $win '07a-startup-on')
        Set-StartupMark $keys
        Click-Ui $win $UiPanel.Nav $UiId.NavActivity
        [void](Wait-Maybe { if ((Find-Ui $win $UiPanel.Page $UiId.Startup) -eq [IntPtr]::Zero) { $true } } 5)
        Click-Ui $win $UiPanel.Nav $UiId.NavSettings
        $said = Wait-Maybe { $t = Read-Ui $win $UiPanel.Page $UiId.StartupText; if ($t -match 'turned off in Windows' -and -not (Get-Checked $win $UiPanel.Page $UiId.Startup)) { $t } } 8
        Check 'switched off in Task Manager, Settings shows the switch off, says so, and how to start it again' ($said -match 'Turn it on here to start it again') (Read-Ui $win $UiPanel.Page $UiId.StartupText)
        $pic = Save-Shot $win '07b-off-in-windows'
        Test-Painted $pic $win 'Start with Windows switched off in Windows' @(@($UiPanel.Page, $UiId.Startup), @($UiPanel.Page, $UiId.StartupText))
        Click-Ui $win $UiPanel.Page $UiId.Startup
        $back = Wait-Maybe { if ($null -eq (Read-StartupMark $keys) -and (Get-Checked $win $UiPanel.Page $UiId.Startup)) { $true } } 8
        Check 'turning it on there deletes Windows'' mark and keeps the entry' ($back -and (Read-StartupEntry $keys) -eq $uiEntry) "mark: $(Read-StartupMark $keys)"
        Click-Ui $win $UiPanel.Page $UiId.Startup
        $off = Wait-Maybe { if ($null -eq (Read-StartupEntry $keys) -and -not (Get-Checked $win $UiPanel.Page $UiId.Startup)) { $true } } 8
        Check 'turning it off deletes the entry' ($null -ne $off) "entry: $(Read-StartupEntry $keys)"

        Open-Repo $win 0
        Choose-Ui $win $UiPanel.Page $UiId.Every 'Default (6 hours)'
        $cfg = Wait-Maybe { $x = Get-Config $c; if ($x -and -not $x.repos[0].interval) { $x } } 8
        Check 'choosing "Default (6 hours)" makes the repository follow the default' ($null -ne $cfg -and (Read-Ui $win $UiPanel.Page $UiId.Every) -eq 'Default (6 hours)') (Read-Ui $win $UiPanel.Page $UiId.Every)

        # ---- Keep in sync
        $checks = Get-MarkCount $c.Report 'job-check'
        Click-Ui $win $UiPanel.Page $UiId.Enabled
        $paused = Wait-Maybe { $n = @(Read-Nav $win); if ($n.Count -ge 1 -and $n[0].Sub -eq 'Paused') { $true } } 8
        $cfg = Get-Config $c
        Check 'turning Keep in sync off is saved, and the list says Paused' ($paused -and $cfg.repos[0].enabled -eq $false) (Show-Nav $win)
        $hero = Read-Hero $win
        Check 'the top of the window says every repository is paused, and Sync now is greyed out' ($hero.Title -eq 'Every repository is paused' -and -not [TssLive]::IsWindowEnabled((Get-Ui $win $UiPanel.Hero $UiId.Sync))) "$($hero.Title) / $($hero.Sub)"
        [void](Save-Shot $win '07-paused')
        Click-Ui $win $UiPanel.Page $UiId.Enabled
        $again = Wait-Maybe { if ((Get-MarkCount $c.Report 'job-check') -gt $checks) { $true } } 30
        Check 'turning it on again checks the repository at once' ($again -and (Get-Checked $win $UiPanel.Page $UiId.Enabled))
        [void](Wait-Maybe { $n = @(Read-Nav $win); if ($n.Count -ge 1 -and $n[0].Sub -match 'up to date$') { $true } } 20)
        Wait-Settled $c

        # ---- a local edit appears by itself, with what can be done about it
        [IO.File]::AppendAllText((Join-Path $c.Skills 'alpha\SKILL.md'), "`nmy own words`n")
        Remove-Item -LiteralPath (Join-Path $c.Skills 'alpha\scripts\hello.ps1')
        # A file of my own in the skill: it is mine, so it is not an edit and is never shown.
        [IO.File]::WriteAllText((Join-Path $c.Skills 'alpha\my-notes.md'), "my notes`n")
        $edited = Wait-Maybe { $r = @(Read-Skills $win | Where-Object { $_.Name -eq 'alpha' -and $_.Tag -eq 'Edited on this PC' }); if ($r.Count -ge 1) { $r[0] } } 40
        Check 'an edit made to a skill shows as "Edited on this PC" without anyone asking' ($null -ne $edited) ((@(Read-Skills $win) | ForEach-Object { "$($_.Name) / $($_.Tag)" }) -join ' ;; ')
        $nav = Wait-Maybe { $n = @(Read-Nav $win); if ($n.Count -ge 1 -and $n[0].Sub -eq "3 skills $dot 1 needs you") { $n[0] } } 10
        $hero = Read-Hero $win
        Check 'the list says the repository needs you, and the top of the window says a skill does' ($null -ne $nav -and $hero.Title -eq '1 skill needs your attention') ((Show-Nav $win) + " / $($hero.Title)")
        if ($null -eq $edited) { return }
        $texts = @($edited.Buttons | ForEach-Object { $_.Text })
        Check 'its row opens by itself, says what changed (not my own file), and offers Show changes, Use the GitHub version and Keep my version' ($edited.Detail -match '^Edited locally' -and $edited.Detail -notmatch 'my-notes' -and $texts -contains 'Show changes' -and $texts -contains 'Use the GitHub version' -and $texts -contains 'Keep my version') ("$($edited.Detail) / " + ($texts -join ', '))
        $editedRow = Get-Ui $win $UiPanel.Page $edited.Id
        [void][TssLive]::SetForegroundWindow($win)
        # Enter/Space must reach the row through normal keyboard navigation, not a notification to the panel.
        for ($i = 0; $i -lt 32; $i++) {
            $current = [TssLive]::Focused($win)
            if ($current -eq $editedRow) { break }
            Send-FocusedKeys $win $current '{TAB}'
            [void](Wait-Maybe { [TssLive]::Focused($win) -ne $current } 1 25)
        }
        Send-FocusedKeys $win $editedRow '{ENTER}'
        $folded = Wait-Maybe {
            Read-Skills $win | Where-Object { $_.Name -eq 'alpha' -and -not $_.Buttons.Count -and $_.Facts -and $_.Detail -eq $edited.Detail }
        } 5
        Check 'Enter closes the repair choices but retains the facts and edit explanation' ($null -ne $folded)
        if ($folded) {
            $was = [TssLive]::CursorAt() -split ','
            try {
                $b = [int[]]([TssLive]::Bounds((Get-Ui $win $UiPanel.Page $edited.Id)) -split ',')
                $nameX = $b[0] + [int](48 * [TssLive]::GetDpiForWindow($win) / 96)
                [void][TssLive]::SetCursorPos($nameX, [int](($b[1] + $b[3]) / 2))
                Test-BodyTip $c $folded
            } finally { [void][TssLive]::SetCursorPos([int]$was[0], [int]$was[1]) }
        }
        Send-FocusedKeys $win (Get-Ui $win $UiPanel.Page $edited.Id) ' '
        $reopened = Wait-Maybe { Read-Ui $win $UiPanel.Page ($edited.Id + 2) } 5
        Check 'Space opens the choices again and restores Show changes' ($reopened -eq 'Show changes')
        [void][TssLive]::Repaint((Get-Ui $win $UiPanel.Page ($edited.Id + 2)))
        $pic = Save-Shot $win '08-skill-edited'
        Test-Painted $pic $win 'an edited skill' @(@($UiPanel.Hero, $UiId.HeroTitle), @($UiPanel.Page, $edited.Id), @($UiPanel.Page, ($edited.Id + 2)))
        Test-Layout $win 'an edited skill'
        Test-TabOrder $win 'an edited skill'
        # Repainting the row must not cover the repair choices beneath it.
        [void][TssLive]::Repaint((Get-Ui $win $UiPanel.Page $edited.Id))
        $again = Save-Shot $win '08b-skill-row-repainted'
        Test-Unchanged $pic $again $win 'an edited skill whose row painted again' @($UiPanel.Page, ($edited.Id + 2)) 'the Show changes button'

        # ---- Show changes, and back
        # Meanwhile GitHub gives alpha a library of 32 reference files and removes its guide; the edit holds that update.
        $library = @(foreach ($i in 1..32) { @{ Path = ('skills/alpha/references/library/component-{0:00}-guide.md' -f $i); Text = ((1..80 | ForEach-Object { "component $i, line $_" }) -join "`n") + "`n" } })
        [void](Push-Upstream $c.Repo $c.Branch ($library + @(@{ Delete = $true; Path = 'skills/alpha/references/guide.md' })) 'live: alpha gains a library and loses its guide')
        [void](Wait-Live { $up = Get-UpstreamFiles $c.Repo $c.Branch; $up.Contains('skills/alpha/references/library/component-32-guide.md') -and -not $up.Contains('skills/alpha/references/guide.md') } 'GitHub to list the pushed change' 30)
        # This repository was added without a token, so the app sees the change only once GitHub's servers do. It asks
        # for the branch's commit and for its listing separately, and each answer may be up to a minute old on its own.
        $head = ([string](Invoke-Gh @('api', "repos/$($c.Repo)/git/ref/heads/$($c.Branch)", '--jq', '.object.sha'))).Trim().ToLowerInvariant()
        [void](Wait-Live { (Get-PublicCommit $c.Repo $c.Branch) -eq $head -and ($up = Get-PublicFiles $c.Repo $c.Branch 'skills').Contains('alpha/references/library/component-32-guide.md') -and -not $up.Contains('alpha/references/guide.md') } 'the answers without a token to show the pushed change' 150 15000)
        Click-Ui $win $UiPanel.Page ($edited.Id + 2)
        $file = Wait-Maybe { $t = Read-Ui $win $UiPanel.Files $UiId.File; if ($t) { $t } } 15
        $inPlace = [TssLive]::Shown([TssLive]::GetDlgItem($win, $UiPanel.Head)) -and -not [TssLive]::Shown([TssLive]::GetDlgItem($win, $UiPanel.Page)) -and -not [TssLive]::Shown([TssLive]::GetDlgItem($win, $UiPanel.Nav))
        Check 'Show changes shows the differences in place of the pages' $inPlace
        $library1 = Read-Ui $win $UiPanel.Files ($UiId.File + 1)
        $script = Read-Ui $win $UiPanel.Files ($UiId.File + 33)
        $skillFile = Read-Ui $win $UiPanel.Files ($UiId.File + 34)
        $past = Read-Ui $win $UiPanel.Files ($UiId.File + 35)
        # The changes read as what taking GitHub's version would do here: my added line and the guide would go, the
        # library and the deleted script would come. My own file sorts first if it is listed, and it is not.
        Check 'the guide GitHub removed, the 32 files it added, the script deleted here and the edited SKILL.md are listed, from this PC to GitHub; my own file is not' ($file -match '^references/guide\.md\nOnly on this PC$' -and $library1 -match '^references/library/component-01-guide\.md\nOnly on GitHub$' -and $script -match '^scripts/hello\.ps1\nOnly on GitHub$' -and $skillFile -match "^SKILL\.md`nChanged $dot 0 added, 1 removed$" -and $null -eq $past) ((@($file, $library1, $script, $skillFile, $past) -join ' / ') -replace "`n", ' / ')
        $head = Read-Panel $win $UiPanel.Head
        Check 'it names the skill, says nothing changes until a choice is made, and offers the choices' ((Read-Ui $win $UiPanel.Head $UiId.Title) -eq 'alpha' -and $head -match 'Nothing changes until you choose' -and $head -match 'Use the GitHub version' -and $head -match 'Keep my version') $head
        $pic = Save-Shot $win '09-changes'
        Test-Painted $pic $win 'the changes of a skill' @(@($UiPanel.Head, $UiId.Back), @($UiPanel.Head, $UiId.Title), @($UiPanel.Head, $UiId.DiffAction), @($UiPanel.Files, $UiId.File))
        Test-Layout $win 'the changes of a skill'
        Click-Ui $win $UiPanel.Files ($UiId.File + 33)
        Start-Sleep -Milliseconds 400
        $pic = Save-Shot $win '09c-only-on-github'
        Test-Painted $pic $win 'a file only on GitHub' @(@($UiPanel.Head, $UiId.Back), @($UiPanel.Head, $UiId.Title))
        Test-ListHover $c $win -Diff
        if ([TssLive]::Shown([TssLive]::GetDlgItem($win, $UiPanel.Head))) {
            Send-Key (Get-Ui $win $UiPanel.Head $UiId.Back) $VK_ESCAPE
        }
        $back = Wait-Maybe { if ([TssLive]::Shown([TssLive]::GetDlgItem($win, $UiPanel.Page)) -and -not [TssLive]::Shown([TssLive]::GetDlgItem($win, $UiPanel.Head))) { $true } } 5
        Check 'Esc goes back to the page of the repository, and nothing changed' ($back -and (Read-Skill $c 'alpha\SKILL.md') -match 'my own words')

        # ---- More, then Remove repository: it asks first
        Click-Ui $win $UiPanel.Page $UiId.More
        $menu = Wait-Menu $tray
        Check 'More opens a menu' ($menu -ne [IntPtr]::Zero)
        if ($menu -ne [IntPtr]::Zero) {
            $items = @([TssLive]::MenuItems($menu))
            Check 'it offers Sync this repository, Open on GitHub, Open skills folder and Remove repository' (($items -join ' | ') -like 'Sync this repository | Open on GitHub | Open skills folder | - | Remove repository*') ($items -join ' | ')
            New-Item -ItemType Directory -Force $Shots | Out-Null
            [void][TssLive]::CaptureScreen($menu, (Join-Path $Shots '10-more-menu.png'), 24)
            Choose-MenuItem $menu ([array]::IndexOf($items, @($items | Where-Object { $_ -like 'Remove repository*' })[0]))
            $ask = Wait-Maybe { $d = Find-Ask $tray; if ($d -ne [IntPtr]::Zero) { $d } } 8
            Check 'Remove repository asks before it does anything' ($null -ne $ask)
            if ($null -ne $ask) {
                Start-Sleep -Milliseconds 400
                [void](Save-Shot $ask '11-remove-asks')
                Note ('the question: ' + (Read-Panel $ask $AskPanel))
                $one = Read-Ui $ask $AskPanel $AskId.Choice
                $two = Read-Ui $ask $AskPanel ($AskId.Choice + 1)
                Check 'it offers two choices, and the one that keeps the skills installed is chosen to start with' ($one -like 'Leave its skills installed*' -and $two -like 'Delete its skills too*' -and (Get-Checked $ask $AskPanel $AskId.Choice) -and -not (Get-Checked $ask $AskPanel ($AskId.Choice + 1))) "$one / $two"
                Send-Key (Get-Ui $ask $AskPanel $AskId.Cancel) $VK_ESCAPE
                $gone = Wait-Maybe { if ((Find-Ask $tray) -eq [IntPtr]::Zero) { $true } } 5
                Check 'Esc cancels the question; the repository and its skills are still there' ($gone -and @(Read-Nav $win).Count -eq 1 -and (Test-Path -LiteralPath (Join-Path $c.Skills 'alpha\SKILL.md')))
            } else { Send-Key $menu $VK_ESCAPE }
        }

        # ---- failed checks: what the window says about each
        $c.Proxy.SetMode('api', 'close')
        Click-Ui $win $UiPanel.Hero $UiId.Sync
        $nav = Wait-Maybe { $n = @(Read-Nav $win); if ($n.Count -ge 1 -and $n[0].Sub -eq 'No connection') { $n[0] } } 30
        Check 'with the network down, Sync now ends with the list saying "No connection"' ($null -ne $nav) (Show-Nav $win)
        $hero = Read-Hero $win
        Check 'the top of the window says the repository can not be read, and why' ($hero.Title -match '^1 repository can.t be read$' -and $hero.Sub -match 'connection') "$($hero.Title) / $($hero.Sub)"
        $lead = Read-Ui $win $UiPanel.Page $UiId.InfoLead
        $why = Read-Ui $win $UiPanel.Page $UiId.InfoText
        Check 'its page says it can not be read and why, with Open Settings and Try again' ($lead -match '^Can.t read this repository\.$' -and $why -match 'connection' -and $null -ne (Read-Ui $win $UiPanel.Page $UiId.Retry) -and $null -ne (Read-Ui $win $UiPanel.Page $UiId.OpenSettings)) "$lead $why"
        Check 'the skills are still listed and on disk' (@(Read-Skills $win).Count -eq 3 -and @(Get-SkillNames $c).Count -eq 3)
        $pic = Save-Shot $win '12-no-connection'
        Test-Painted $pic $win 'a failed check' @(@($UiPanel.Hero, $UiId.HeroTitle), @($UiPanel.Page, $UiId.InfoLead), @($UiPanel.Page, $UiId.InfoText), @($UiPanel.Page, $UiId.Retry))
        Test-Layout $win 'a failed check'
        Test-TabOrder $win 'a failed check'
        foreach ($case in @(@('404', 'Not found'), @('redirect', 'Repository moved'))) {
            $c.Proxy.SetMode('api', $case[0])
            $status = $case[1]
            Click-Ui $win $UiPanel.Page $UiId.Retry
            $nav = Wait-Maybe { $n = @(Read-Nav $win); if ($n.Count -ge 1 -and $n[0].Sub -eq $status) { $n[0] } } 30
            Check "when GitHub answers $($case[0]), Try again ends with the list saying `"$status`"" ($null -ne $nav) (Show-Nav $win)
            Start-Sleep -Milliseconds 400
            Note ("the page ($status): " + (Read-Ui $win $UiPanel.Page $UiId.InfoLead) + ' ' + ((Read-Ui $win $UiPanel.Page $UiId.InfoText) -replace '\s+', ' '))
        }
        $c.Proxy.SetMode('api', 'pass')
        Click-Ui $win $UiPanel.Page $UiId.Retry
        # Alpha is still edited, so a repository that is well again needs the user for that.
        $healed = Wait-Maybe { $n = @(Read-Nav $win); if ($n.Count -ge 1 -and $n[0].Sub -eq "3 skills $dot 1 needs you") { $n[0] } } 30
        Check 'when GitHub answers again, Try again clears the failure, and the edited skill is what the repository needs you for' ($null -ne $healed -and $null -eq (Read-Ui $win $UiPanel.Page $UiId.InfoLead)) (Show-Nav $win)

        # ---- the tray menu
        $menu = Show-TrayMenu $tray
        Check 'the tray menu opens' ($menu -ne [IntPtr]::Zero)
        if ($menu -ne [IntPtr]::Zero) {
            $items = @([TssLive]::MenuItems($menu))
            Check 'the menu offers Open Tiny Skill Sync (the default), Sync now, and Quit after a separator' (($items -join ' | ') -eq 'Open Tiny Skill Sync (default) | Sync now | - | Quit') ($items -join ' | ')
            New-Item -ItemType Directory -Force $Shots | Out-Null
            [void][TssLive]::CaptureScreen($menu, (Join-Path $Shots '13-tray-menu.png'), 24)
            Hide-TrayMenu $tray
            $closed = Wait-Maybe { if ((Find-LiveWindow $tray.Id $MENU_CLASS -Visible) -eq [IntPtr]::Zero) { $true } } 4
            Check 'the menu goes away when it is dismissed' $closed
        }

        # ---- the window keeps the size it is given
        $dpi = [int][TssLive]::GetDpiForWindow($win)
        [void][TssLive]::SetWindowPos($win, [IntPtr]::Zero, 0, 0, [int](860 * $dpi / 96), [int](600 * $dpi / 96), 0x16)
        [void][TssLive]::PostMessageW($win, $WM_EXITSIZEMOVE, [IntPtr]::Zero, [IntPtr]::Zero)
        $client = ([TssLive]::Client($win)) -split ','
        $want = '{0} {1}' -f [math]::Floor([int]$client[0] * 96 / $dpi), [math]::Floor([int]$client[1] * 96 / $dpi)
        $sizeFile = Join-Path $c.Home 'window.txt'
        $saved = Wait-Maybe { $t = $(try { [IO.File]::ReadAllText($sizeFile).Trim() } catch { '' }); if ($t -eq $want) { $t } } 5
        Check 'a window given a new size remembers it' ($null -ne $saved) ("window.txt: " + $(try { [IO.File]::ReadAllText($sizeFile).Trim() } catch { '-' }) + "; wanted $want at $dpi dpi")
        $pic = Save-Shot $win '14-smaller'
        Test-Painted $pic $win 'a smaller window' @(@($UiPanel.Hero, $UiId.Sync), @($UiPanel.Nav, $UiId.NavSettings), @($UiPanel.Page, $UiId.More))
        Test-Layout $win 'a smaller window'
        Test-TabOrder $win 'a smaller window'

        # ---- closing, and starting the app a second time
        Send-Key (Get-Ui $win $UiPanel.Nav $UiId.NavAdd) $VK_ESCAPE
        $closed = Wait-Maybe { if ((Find-LiveWindow $tray.Id $WINDOW_CLASS) -eq [IntPtr]::Zero) { $true } } 5
        Check 'Esc closes the window, and the app keeps running' ($closed -and -not $tray.HasExited)
        $second = Start-LiveTray $c.Root @() (Get-EnvOf $c @{ TSS_BENCH_REPORT = $c.Report })
        $ended = $second.WaitForExit(8000)
        if (-not $ended) { Stop-Process -Id $second.Id -Force }
        $win = Wait-Maybe { $w = Find-LiveWindow $tray.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 8
        Check 'starting the app a second time ends the new copy and opens the window of the running one' ($ended -and $null -ne $win) "the second copy ended: $ended"
        if ($null -ne $win) {
            Start-Sleep -Milliseconds 500
            $client = ([TssLive]::Client($win)) -split ','
            $dpi = [int][TssLive]::GetDpiForWindow($win)
            $now = @([math]::Floor([int]$client[0] * 96 / $dpi), [math]::Floor([int]$client[1] * 96 / $dpi))
            $was = @($want -split ' ' | ForEach-Object { [int]$_ })
            Check 'it opens at the size it was given, with the repository still there' ([math]::Abs($now[0] - $was[0]) -le 1 -and [math]::Abs($now[1] - $was[1]) -le 1 -and @(Read-Nav $win).Count -eq 1) ("$($now -join ' ') for $want; " + (Show-Nav $win))

            # ---- bad input in the Add question
            Click-Ui $win $UiPanel.Nav $UiId.NavAdd
            $ask = Wait-Maybe { $d = Find-Ask $tray; if ($d -ne [IntPtr]::Zero) { $d } } 8
            Check 'Add repository asks again' ($null -ne $ask)
            if ($null -ne $ask) {
                $edit = Get-Ui $ask $AskPanel $AskId.Edit
                Set-UiText $edit 'not a repo'
                $hint = Wait-Maybe { $t = Read-Ui $ask $AskPanel $AskId.Hint; if ($t -and $t -notlike 'owner/name*') { $t } } 5
                Click-Ui $ask $AskPanel $AskId.Ok
                Start-Sleep -Milliseconds 600
                Check 'an address that is not a repository is refused: the question stays, says why, and keeps what was typed' ($null -ne $hint -and (Find-Ask $tray) -ne [IntPtr]::Zero -and [TssLive]::Text($edit) -eq 'not a repo' -and @(Read-Nav $win).Count -eq 1) $hint
                Set-UiText $edit (Get-Address $c)
                $hint = Wait-Maybe { $t = Read-Ui $ask $AskPanel $AskId.Hint; if ($t -match 'already set up') { $t } } 5
                Click-Ui $ask $AskPanel $AskId.Ok
                Start-Sleep -Milliseconds 600
                Check 'adding the same repository twice is refused, and says so' ($null -ne $hint -and (Find-Ask $tray) -ne [IntPtr]::Zero -and @(Read-Nav $win).Count -eq 1) $hint
                [void](Save-Shot $ask '15-refused')

                # ---- a repository with no skills at its top: what the window says
                Set-UiText $edit "https://github.com/$($c.Repo)"
                [void](Wait-Maybe { $t = Read-Ui $ask $AskPanel $AskId.Hint; if ($t -match 'top of the repository') { $t } } 5)
                Click-Ui $ask $AskPanel $AskId.Ok
                $nav = Wait-Maybe { $n = @(Read-Nav $win); if ($n.Count -eq 2 -and $n[1].Sub -eq 'No skills found') { $n[1] } } 40
                Check 'a repository with no skills at its top says "No skills found" in the list' ($null -ne $nav) (Show-Nav $win)
                $lead = Read-Ui $win $UiPanel.Page $UiId.InfoLead
                $why = Read-Ui $win $UiPanel.Page $UiId.InfoText
                Note "its page: $lead $($why -replace '\s+', ' ')"
                Check 'its page says so, and what to do about it, with Try again' ($lead -eq 'No skills found.' -and $why -match 'No skills were found' -and $null -ne (Read-Ui $win $UiPanel.Page $UiId.Retry)) "$lead $why"
                $pic = Save-Shot $win '16-no-skills-found'
                Test-Painted $pic $win 'a repository without skills' @(@($UiPanel.Page, $UiId.InfoLead), @($UiPanel.Page, $UiId.InfoText), @($UiPanel.Nav, ($UiId.NavRepo + 1)))
                Test-Layout $win 'a repository without skills'
            }
        }

        $box = Find-LiveWindow $tray.Id '#32770'
        Check 'the app showed no error box' ($box -eq [IntPtr]::Zero)
        Send-LiveCommand $tray $CMD_QUIT
        Check 'Quit ends the app' ($tray.WaitForExit(5000))
    } finally {
        if (-not $Keep) { try { Remove-StartupKeys $keys } catch { Note "could not delete $($keys.Root): $_" } }
        Close-Env $c
    }
}

# ---------------------------------------------------------------- signin

# Start with Windows. A copy started with TSS_HOME keeps its entry in keys of its own under $TestKeyRoot, never in
# Windows' keys, and the scenario starts the app from that entry the way Windows does when the user signs in: the
# command line exactly as written, from System32. Windows' own keys are read before and after, to prove they were not
# touched. Copies in deep folders show how a path too long for Windows to start is named by its short (8.3) form.
function Test-Signin {
    $c = New-Env 'signin' -NoBranch
    $keys = Get-StartupKeys $c.Home
    $running = New-Object System.Collections.Generic.List[object]
    try {
        $real = Get-RealStartup
        $mainTray = [TssLive]::LongPath((Join-Path $c.Root 'bin\tiny-skill-sync.exe'))
        $entry = "`"$mainTray`" --background"
        $quiet = @{ TSS_HOME = $c.Home; TSS_SKILLS_DIR = $c.Skills; TSS_BENCH_REPORT = $null }
        $plain = 'Starts in the tray when you sign in to Windows, without opening its window.'
        $other = { param([string]$Path) "Windows starts another copy when you sign in: $Path. Run 'tss startup on' to start this copy instead." }
        # What tss startup printed: exit 0, the state in a few words, then the note.
        $says = {
            param($r, [string]$Summary, [string]$Said)
            $lines = @($r.Text -split "`r?`n" | Where-Object { $_.Trim() })
            $r.Code -eq 0 -and $lines.Count -eq 2 -and $lines[0] -eq "Start with Windows: $Summary" -and $lines[1] -eq $Said
        }
        $inTray = { param([Diagnostics.Process]$P) Wait-Maybe { if ((Find-LiveWindow $P.Id $TRAY_CLASS) -ne [IntPtr]::Zero) { $true } } 10 }

        # ---- the entry, through tss startup
        $r = Tss $c @('startup')
        Check 'at first tss startup says off, and what turning it on does; looking writes nothing' ((& $says $r 'off' $plain) -and -not (Test-Path -LiteralPath "HKCU:\$($keys.Root)")) ''
        $r = Tss $c @('startup', 'on')
        $now = Read-StartupEntry $keys
        Check 'tss startup on: on, and the entry starts this copy''s tray app with --background' ((& $says $r 'on' $plain) -and $now -eq $entry -and $null -eq (Read-StartupMark $keys)) $now
        Set-StartupMark $keys
        $r = Tss $c @('startup')
        Check 'switched off in Task Manager: tss startup says off, and how to start it again' (& $says $r 'off' "It is turned off in Windows' startup apps (Task Manager or Settings). Run 'tss startup on' to start it again.") ''
        $r = Tss $c @('startup', 'on')
        Check 'tss startup on then deletes Windows'' mark and keeps the entry' ((& $says $r 'on' $plain) -and $null -eq (Read-StartupMark $keys) -and (Read-StartupEntry $keys) -eq $entry) "mark: $(Read-StartupMark $keys)"

        # ---- started from the entry with nothing set up yet: the tray only
        $p = Get-EntryProcess (Start-Entry $entry $quiet)
        $running.Add($p)
        $tray = & $inTray $p
        Start-Sleep -Seconds 3
        Check 'started from the entry with nothing set up, the app is in the tray and opens no window' ($tray -and -not $p.HasExited -and (Find-LiveWindow $p.Id $WINDOW_CLASS -Visible) -eq [IntPtr]::Zero) "pid $($p.Id)"
        Stop-LiveTray $p
        Check 'and quits when asked' ($p.HasExited -and $p.ExitCode -eq 0) "exit code $($p.ExitCode)"

        # ---- started from the entry with a repository: for 30 s it neither looks at the skills folder nor checks
        # GitHub, so that Windows can finish starting; then it looks, and checks
        Add-Repo $c
        Remove-Item -LiteralPath $c.Report -Force -ErrorAction SilentlyContinue
        $withReport = $quiet.Clone()
        $withReport['TSS_BENCH_REPORT'] = $c.Report
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $c.Tray = Get-EntryProcess (Start-Entry $entry $withReport)
        $rest = 15000 - $clock.ElapsedMilliseconds
        if ($rest -gt 0) { Start-Sleep -Milliseconds $rest }
        $jobs = @(Read-Marks $c.Report | Where-Object { $_.Mark -like 'job-*' } | ForEach-Object { $_.Mark })
        $settingsLoads = @($jobs | Where-Object { $_ -eq 'job-settings' })
        $workJobs = @($jobs | Where-Object { $_ -ne 'job-settings' })
        $names = @(Get-SkillNames $c)
        Check 'for its first 15 s it loads settings but does not look at the skills folder or check GitHub' ($settingsLoads.Count -eq 1 -and $workJobs.Count -eq 0 -and $names.Count -eq 0) "jobs: $($jobs -join ','); skills: $($names -join ',')"
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -ge 1 } 'the first look at the skills folder' 75)
        $look = @(Read-Marks $c.Report | Where-Object { $_.Mark -eq 'job-startup' })[0]
        Check ('its first look at the skills folder came after that wait: it ended {0:N1} s after the start' -f ($look.AtMs / 1000)) ($look.AtMs -ge 29000 -and $look.AtMs -lt 60000) 'between 29 and 60 s'
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-poll') -ge 1 } 'the first check of GitHub' 75)
        $poll = @(Read-Marks $c.Report | Where-Object { $_.Mark -eq 'job-poll' })[0]
        Check ('then it checked GitHub: the check ended {0:N1} s after the start' -f ($poll.AtMs / 1000)) ($poll.AtMs -gt $look.AtMs -and $poll.AtMs -lt 90000) 'after the look, within 90 s'
        Wait-Settled $c
        Note ('app marks: ' + ((@(Read-Marks $c.Report) | ForEach-Object { '{0} at {1:N0} ms' -f $_.Mark, $_.AtMs }) -join ', '))
        $diff = Compare-WithUpstream $c.Skills $c.Repo $c.Ref 'skills'
        $names = (Get-SkillNames $c) -join ','
        Check 'then it installed alpha, beta and gamma, equal to upstream' ($diff.Count -eq 0 -and $names -eq 'alpha,beta,gamma') "$names; $(($diff | Select-Object -First 4) -join '; ')"
        Check 'and it still has no window open' ((Find-LiveWindow $c.Tray.Id $WINDOW_CLASS -Visible) -eq [IntPtr]::Zero) ''

        # ---- started from the entry again while it runs: the second copy ends at once, quietly
        $again = Start-Entry $entry $quiet
        try {
            $code = [TssLive]::Exited($again.Handle, 8000)
            if ($code -lt 0) { [void][TssLive]::TerminateProcess($again.Handle, 1) }
        } finally { [void][TssLive]::CloseHandle($again.Handle) }
        Check 'started from the entry again while it runs, the second copy ends at once with 0 and shows nothing' ($code -eq 0 -and (Find-LiveWindow $c.Tray.Id $WINDOW_CLASS -Visible) -eq [IntPtr]::Zero) "exit code $code"

        # ---- started by hand while it runs: the running copy opens its window
        $byHand = Start-LiveTray $c.Root
        $running.Add($byHand)
        $ended = $byHand.WaitForExit(8000)
        $win = Wait-Maybe { $w = Find-LiveWindow $c.Tray.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 10
        Check 'started by hand while it runs, the second copy ends with 0 and the running copy opens its window' ($ended -and $byHand.ExitCode -eq 0 -and $null -ne $win) "ended: $ended"
        Close-Window $c.Tray
        Stop-LiveTray $c.Tray
        Check 'the copy started at sign-in quits when asked' ($c.Tray.HasExited -and $c.Tray.ExitCode -eq 0) "exit code $($c.Tray.ExitCode)"

        # ---- started from the entry, then by hand while it waits: someone is there to see the window, so it looks at
        # once; checking GitHub still waits for the network
        Remove-Item -LiteralPath $c.Report -Force -ErrorAction SilentlyContinue
        $c.Tray = Get-EntryProcess (Start-Entry $entry $withReport)
        [void](& $inTray $c.Tray)
        $byHand = Start-LiveTray $c.Root
        $running.Add($byHand)
        $ended = $byHand.WaitForExit(8000)
        $win = Wait-Maybe { $w = Find-LiveWindow $c.Tray.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 10
        [void](Wait-Live { (Get-MarkCount $c.Report 'job-startup') -ge 1 } 'the first look once the window opened' 20)
        $look = @(Read-Marks $c.Report | Where-Object { $_.Mark -eq 'job-startup' })[0]
        $polls = Get-MarkCount $c.Report 'job-poll'
        Check ('started by hand while the copy started at sign-in waits, its window opens and it looks at once ({0:N1} s after its start), and checks nothing yet' -f ($look.AtMs / 1000)) ($ended -and $byHand.ExitCode -eq 0 -and $null -ne $win -and $look.AtMs -lt 15000 -and $polls -eq 0) "ended: $ended; window: $($null -ne $win); checks: $polls"
        Close-Window $c.Tray
        Stop-LiveTray $c.Tray

        # ---- settings that cannot be read: the window opens even at sign-in, as nothing is checked until they are fixed
        $cfgFile = Join-Path $c.Home 'config.json'
        $saved = [IO.File]::ReadAllText($cfgFile)
        try {
            [IO.File]::WriteAllText($cfgFile, '{ not json')
            $p = Get-EntryProcess (Start-Entry $entry $quiet)
            $running.Add($p)
            $win = Wait-Maybe { $w = Find-LiveWindow $p.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 10
            $hero = if ($null -ne $win) { Read-Hero $win } else { $null }
            Check 'with settings that cannot be read, a start from the entry opens the window to say so' ($null -ne $win) $(if ($hero) { "$($hero.Title) / $($hero.Sub)" } else { '' })
            Stop-LiveTray $p
        } finally { [IO.File]::WriteAllText($cfgFile, $saved) }

        # ---- a copy in a folder whose path is longer than Windows starts a program from: its entry names it by its short
        # (8.3) path
        $deep = "$($c.Root)\sf"
        for ($i = 1; ($deep.Length + '\tiny-skill-sync.exe'.Length) -le 262; $i++) { $deep += ('\a-rather-long-folder-name-{0:d2}' -f $i) }
        Copy-AppTo $c $deep
        $deepTray = [TssLive]::LongPath("$deep\tiny-skill-sync.exe")
        $shortTray = [TssLive]::ShortPath($deepTray)
        $shortTss = [TssLive]::ShortPath("$deep\tss.exe")
        Note ('a copy whose tray app is at a path of {0} characters; its short path has {1}' -f $deepTray.Length, $(if ($shortTray) { $shortTray.Length } else { 'none' }))
        if (-not $shortTray -or -not $shortTss -or $shortTray -eq $deepTray) {
            Note 'this volume keeps no short (8.3) names, so a copy in a long folder cannot be started at sign-in; not checked'
        } else {
            $r = Tss $c @('startup') -Exe $shortTss
            Check 'a copy in a long folder: tss startup there says off, and names the copy Windows starts' (& $says $r 'off' (& $other $mainTray)) ''
            $r = Tss $c @('startup', 'on') -Exe $shortTss
            $now = Read-StartupEntry $keys
            $fits = $now -eq "`"$shortTray`" --background" -and $now.Length -le 260 -and [TssLive]::LongPath($shortTray) -eq $deepTray
            Check 'tss startup on there writes an entry that names it by its short path, which fits' ((& $says $r 'on' $plain) -and $fits) "$($now.Length) characters: $now"
            $r = Tss $c @('startup')
            Check 'and this copy''s tss startup names the other copy by that path' (& $says $r 'off' (& $other $shortTray)) ''

            $p = Get-EntryProcess (Start-Entry $now $quiet)
            $running.Add($p)
            $tray = & $inTray $p
            Start-Sleep -Seconds 2
            $ran = try { [TssLive]::LongPath($p.MainModule.FileName) } catch { "unknown: $($_.Exception.Message)" }
            Check 'Windows can start that copy from its entry: it is in the tray, with no window' ($tray -and -not $p.HasExited -and (Find-LiveWindow $p.Id $WINDOW_CLASS -Visible) -eq [IntPtr]::Zero) "running $ran"
            Stop-LiveTray $p
            Check 'and it quits when asked' ($p.HasExited -and $p.ExitCode -eq 0) "exit code $($p.ExitCode)"

            $r = Tss $c @('startup', 'on')
            Check 'tss startup on in this copy takes the entry back' ((& $says $r 'on' $plain) -and (Read-StartupEntry $keys) -eq $entry) (Read-StartupEntry $keys)
            $r = Tss $c @('startup', 'off') -Exe $shortTss
            Check 'tss startup off in the other copy leaves this copy''s entry alone' ((& $says $r 'off' (& $other $mainTray)) -and (Read-StartupEntry $keys) -eq $entry) (Read-StartupEntry $keys)
        }

        # ---- a copy in a folder so deep that even its short path is too long for an entry: the folder's short path
        # leaves room for tss.exe (259 characters at most) but not for the entry (260)
        $tl = "$($c.Root)\tl"
        [void][IO.Directory]::CreateDirectory($tl)
        $levels = [math]::Max(0, [math]::Ceiling((233 - ([TssLive]::ShortPath($tl)).Length) / 9))
        $dir = $tl + ('\dddddddd' * $levels)
        Copy-AppTo $c $dir
        $shortDir = [TssLive]::ShortPath($dir)
        $shortCommand = "`"$([TssLive]::ShortPath("$dir\tiny-skill-sync.exe"))`" --background"
        Note ('a copy in a folder whose short path has {0} characters; its entry would have {1}' -f $shortDir.Length, $shortCommand.Length)
        if ($shortDir.Length -lt 233 -or $shortDir.Length -gt 251 -or $shortCommand.Length -le 260) { throw "the too-deep folder is not as intended: its short path has $($shortDir.Length) characters" }
        $deepTss = "$shortDir\tss.exe"
        $r = Tss $c @('startup') -Exe $deepTss
        Check 'a copy in a folder too deep for an entry: tss startup there says not possible, and why' (& $says $r 'not possible' 'This copy is in a folder whose path is too long for Windows to start it. Move it to a shorter folder to use this.') ''
        $r = Tss $c @('startup', 'on') -Exe $deepTss
        $lines = @($r.Text -split "`r?`n" | Where-Object { $_.Trim() })
        $refused = $r.Code -eq 2 -and $lines.Count -eq 1 -and $lines[0] -eq "tss: Start with Windows could not be turned on: this copy's path is too long for Windows to start it"
        Check 'and tss startup on there fails with exit 2, says why, and leaves the entry as it was' ($refused -and (Read-StartupEntry $keys) -eq $entry) "exit $($r.Code)"

        # ---- off
        $r = Tss $c @('startup', 'off')
        Check 'tss startup off: off, with no entry and no mark left' ((& $says $r 'off' $plain) -and $null -eq (Read-StartupEntry $keys) -and $null -eq (Read-StartupMark $keys)) ''
        $after = Get-RealStartup
        Check 'Windows'' own startup keys were not touched' ($after -eq $real) $after
    } finally {
        foreach ($p in $running) { try { Stop-LiveTray $p } catch { } }
        if (-not $Keep) { try { Remove-StartupKeys $keys } catch { Note "could not delete $($keys.Root): $_" } }
        Close-Env $c
    }
}

# Copies the app's two programs into a folder at any depth, which Windows PowerShell makes only through \\?\.
function Copy-AppTo($c, [string]$Dir) {
    [void][IO.Directory]::CreateDirectory('\\?\' + $Dir)
    foreach ($exe in 'tiny-skill-sync.exe', 'tss.exe') { [IO.File]::Copy('\\?\' + (Join-Path $c.Root "bin\$exe"), "\\?\$Dir\$exe") }
}

# ---- more scenarios go above this line

# ---------------------------------------------------------------- run

$scenarios = [ordered]@{
    'install'      = 'Test-Install'
    'places'       = 'Test-Places'
    'portable'     = 'Test-Portable'
    'changes'      = 'Test-Changes'
    'local-edit'   = 'Test-LocalEdit'
    'converged'    = 'Test-Converged'
    'edit-stopped' = 'Test-EditStopped'
    'holds'        = 'Test-Holds'
    'unmanaged'    = 'Test-Unmanaged'
    'private'      = 'Test-Private'
    'hostile'      = 'Test-Hostile'
    'names'        = 'Test-Names'
    'failures'     = 'Test-Failures'
    'stale'        = 'Test-Stale'
    'empty'        = 'Test-Empty'
    'signin'       = 'Test-Signin'
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
