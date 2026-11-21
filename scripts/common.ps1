# Shared by the scripts in this folder: . "$PSScriptRoot\common.ps1"
# Works in Windows PowerShell 5.1 and PowerShell 7.

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
# The scripts' own settings and skills folders. Never the real ones.
$Sandbox = Join-Path $RepoRoot '.tss-test'
# Folders the tests make in %TEMP%. A test that fails can leave its folder behind.
$TestTempPattern = '^tss-(it|apply|fsx|config|engine|state|verify|winfs)-'
# Throwaway folders these scripts make in %TEMP%. They delete them, unless stopped half way.
$ScriptTempPattern = '^tss-(dryrun|bench|e2e|soak|demo|relaunch)-'
# Files the live scripts (e2e, soak) make in %TEMP% to talk to GitHub.
$ScriptTempFilePattern = '^tss-gh-body-[0-9a-f]+\.json$'
# Where a copy started with TSS_HOME keeps its startup entry, one key per app folder (autostart::TEST_ROOT). A copy
# with an app folder of its own never writes Windows' Run key.
$TestRegistryRoot = 'HKCU:\Software\tiny-skill-sync-test'

# The long name of an existing folder. %TEMP% can be spelled with 8.3 short names (GitHub's runners use
# C:\Users\RUNNER~1\...), and the app refuses such an alias for an Agents or Instructions folder.
function Get-LongPath([string]$Path) {
    if ($Path -notmatch '~') { return $Path }
    if (-not ('TssScripts.LongPath' -as [type])) {
        Add-Type -Namespace TssScripts -Name LongPath -MemberDefinition @'
[DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern uint GetLongPathNameW(string shortPath, System.Text.StringBuilder longPath, uint size);
'@
    }
    $long = New-Object System.Text.StringBuilder 1024
    $n = [TssScripts.LongPath]::GetLongPathNameW($Path, $long, [uint32]$long.Capacity)
    if ($n -eq 0 -or $n -ge $long.Capacity) { throw "cannot find the long name of $Path" }
    $long.ToString()
}

# Where the scripts and tests make their throwaway folders.
$TempRoot = Get-LongPath $env:TEMP

function Use-Cargo {
    if (Get-Command cargo -ErrorAction SilentlyContinue) { return }
    $bin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (-not (Test-Path (Join-Path $bin 'cargo.exe'))) {
        throw 'cargo was not found. Install Rust first (see CONTRIBUTING.md).'
    }
    $env:Path = "$bin;$env:Path"
}

# Runs a program and stops the script if it fails. Its error output is shown, not treated as a script error.
function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments = @())
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Exe @Arguments } finally { $ErrorActionPreference = $saved }
    if ($LASTEXITCODE -ne 0) { throw "'$Exe $($Arguments -join ' ')' failed with exit code $LASTEXITCODE" }
}

# The folder of the executables for a build: 'debug' or 'release'.
function Get-ExeDir([string]$Build) {
    Join-Path $RepoRoot "target\$Build"
}

# A fresh install, not just fresh settings: a portable export or an app update acts beside the executables.
# $Build 'debug' copies the debug pair instead (relaunch-smoke.ps1 needs its test commands).
function New-LiveRoot([string]$Prefix, [string]$Build = 'release') {
    $root = Join-Path $TempRoot ("tss-$Prefix-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $root -ErrorAction Stop | Out-Null
    try {
        foreach ($d in 'home', 'skills', 'bin', 'out') { New-Item -ItemType Directory -Path (Join-Path $root $d) | Out-Null }
        $from = Get-ExeDir $Build
        foreach ($exe in 'tiny-skill-sync.exe', 'tss.exe') {
            $built = Join-Path $from $exe
            if (-not (Test-Path -LiteralPath $built -PathType Leaf)) { throw "$exe is not built. Run scripts\build.ps1$(if ($Build -eq 'debug') { ' -DebugBuild' }) first." }
            Copy-Item -LiteralPath $built -Destination (Join-Path $root 'bin')
        }
        $root
    } catch {
        Remove-Item -LiteralPath $root -Recurse -Force
        throw
    }
}

# No gh even on a rate limit or a Settings visit. win::proc searches the exe's folder and Windows' folders
# in addition to PATH. Refuse an unusual installation of gh there rather than using anyone's sign-in.
function Get-AnonymousAppEnv([string]$Root) {
    $bin = Join-Path $Root 'bin'
    $windows = [Environment]::GetFolderPath('Windows')
    foreach ($dir in @($bin, $windows, [Environment]::SystemDirectory, (Join-Path $windows 'Sysnative'))) {
        if (Test-Path -LiteralPath (Join-Path $dir 'gh.exe')) { throw 'Cannot isolate this run from the GitHub CLI.' }
    }
    @{
        PATH = $bin; GH_CONFIG_DIR = (Join-Path $Root 'gh')
        GH_TOKEN = $null; GITHUB_TOKEN = $null; GH_ENTERPRISE_TOKEN = $null; GITHUB_ENTERPRISE_TOKEN = $null
        TSS_API_BASE = $null; TSS_RAW_BASE = $null; TSS_BENCH_REPORT = $null
    }
}

function Format-Arg([string]$Text) {
    if ($Text -notmatch '[\s"]' -and $Text -ne '') { return $Text }
    '"' + ($Text -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}

function New-LiveStartInfo($Root, [string]$Exe, [string[]]$AppArgs, [hashtable]$Env) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $Root "bin\$Exe"
    $psi.Arguments = ($AppArgs | ForEach-Object { Format-Arg $_ }) -join ' '
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['TSS_HOME'] = Join-Path $Root 'home'
    $psi.EnvironmentVariables['TSS_SKILLS_DIR'] = Join-Path $Root 'skills'
    foreach ($key in @($Env.Keys)) {
        if ($null -eq $Env[$key]) { $psi.EnvironmentVariables.Remove($key) } else { $psi.EnvironmentVariables[$key] = [string]$Env[$key] }
    }
    # These are not scenario options. Even an inherited value or a bad override must not enable release actions.
    $psi.EnvironmentVariables['TSS_NO_APP_UPDATES'] = '1'
    $psi.EnvironmentVariables['TSS_NO_SAVED_TOKEN'] = '1'
    $psi
}

# Runs the command line on the throwaway install and returns its exit code and output. $Exe runs another
# copy of tss.exe on the same folders instead (the long-path startup test).
function Invoke-LiveTss($Root, [string[]]$TssArgs, [hashtable]$Env = @{}, [int]$TimeoutSec = 240, [string]$Exe = '', [string]$InputLine = '') {
    Assert-NotTokenChange $TssArgs
    $psi = New-LiveStartInfo $Root 'tss.exe' $TssArgs $Env
    if ($Exe) { $psi.FileName = $Exe }
    Invoke-LiveConsole $psi "tss $($TssArgs -join ' ')" $TimeoutSec $InputLine
}

function Invoke-LiveConsole($psi, [string]$Label, [int]$TimeoutSec, [string]$InputLine = '') {
    $psi.RedirectStandardInput = [bool]$InputLine
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [Text.Encoding]::UTF8
    $p = [Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    if ($InputLine) { $p.StandardInput.WriteLine($InputLine); $p.StandardInput.Close() }
    if (-not $p.WaitForExit($TimeoutSec * 1000)) {
        Stop-Process -Id $p.Id -Force
        throw "$Label did not finish within $TimeoutSec s and was stopped."
    }
    $p.WaitForExit()
    [pscustomobject]@{ Code = $p.ExitCode; Out = $out.Result; Text = ($out.Result + $err.Result).TrimEnd() }
    $p.Dispose()
}

# Runs $Body with TSS_HOME and TSS_SKILLS_DIR pointing at the sandbox, so it and every program it starts use the
# sandbox folders. Dev runs must not update the build in target\. The caller's values are put back afterwards.
function Invoke-InSandbox {
    param([scriptblock]$Body, [string]$Root = $Sandbox)
    $saved = $env:TSS_HOME, $env:TSS_SKILLS_DIR, $env:TSS_NO_APP_UPDATES
    $env:TSS_HOME = Join-Path $Root 'home'
    $env:TSS_SKILLS_DIR = Join-Path $Root 'skills'
    $env:TSS_NO_APP_UPDATES = '1'
    try {
        New-Item -ItemType Directory -Force $env:TSS_HOME | Out-Null
        & $Body
    } finally { $env:TSS_HOME, $env:TSS_SKILLS_DIR, $env:TSS_NO_APP_UPDATES = $saved }
}

# The GitHub token in Windows Credential Manager is shared by every copy of the app, sandbox or not.
function Assert-NotTokenChange([string[]]$Arguments) {
    if ($Arguments.Count -ge 2 -and $Arguments[0] -eq 'token' -and $Arguments[1] -in 'set', 'clear') {
        throw "'tss token $($Arguments[1])' would change the real saved GitHub token, which the sandbox shares. Run target\release\tss.exe directly if you mean it."
    }
}
