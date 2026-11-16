# Shared by the scripts in this folder: . "$PSScriptRoot\common.ps1"
# Works in Windows PowerShell 5.1 and PowerShell 7.

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
# The scripts' own settings and skills folders. Never the real ones.
$Sandbox = Join-Path $RepoRoot '.tss-test'
# Folders the tests make in %TEMP%. A test that fails can leave its folder behind.
$TestTempPattern = '^tss-(it|apply|fsx|config|engine|state|verify|winfs)-'
# Throwaway folders these scripts make in %TEMP%. They delete them, unless stopped half way.
$ScriptTempPattern = '^tss-(dryrun|bench)-'

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

# Runs $Body with TSS_HOME and TSS_SKILLS_DIR pointing at the sandbox, so it and every program it starts use the
# sandbox folders. The caller's values are put back afterwards.
function Invoke-InSandbox {
    param([scriptblock]$Body, [string]$Root = $Sandbox)
    $saved = $env:TSS_HOME, $env:TSS_SKILLS_DIR
    $env:TSS_HOME = Join-Path $Root 'home'
    $env:TSS_SKILLS_DIR = Join-Path $Root 'skills'
    New-Item -ItemType Directory -Force $env:TSS_HOME | Out-Null
    try { & $Body } finally { $env:TSS_HOME, $env:TSS_SKILLS_DIR = $saved }
}

# The GitHub token in Windows Credential Manager is shared by every copy of the app, sandbox or not.
function Assert-NotTokenChange([string[]]$Arguments) {
    if ($Arguments.Count -ge 2 -and $Arguments[0] -eq 'token' -and $Arguments[1] -in 'set', 'clear') {
        throw "'tss token $($Arguments[1])' would change the real saved GitHub token, which the sandbox shares. Run target\release\tss.exe directly if you mean it."
    }
}
