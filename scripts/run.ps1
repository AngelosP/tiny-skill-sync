<#
.SYNOPSIS
  Runs the tray app on the sandbox: its own settings and skills folders under .tss-test\, never the real ones.

.DESCRIPTION
  Builds first (a debug build unless -Release). One copy of the app runs per settings folder, so the sandbox copy
  runs beside the real one, if any, with its own tray icon. The first start opens the settings window. Running this
  script again while the sandbox copy runs opens its settings window.

  The saved GitHub token is shared with the real app: do not use Set or Clear in the sandbox unless you mean it.

.PARAMETER Release
  Run the release build (what users run) instead of the debug build.

.PARAMETER Fresh
  Close the sandbox copy and delete the whole sandbox first: settings, record and installed skills.

.PARAMETER Quit
  Close the sandbox copy, then stop.

.EXAMPLE
  scripts\run.ps1
  scripts\run.ps1 -Release -Fresh
  scripts\run.ps1 -Quit
#>
param([switch]$Release, [switch]$Fresh, [switch]$Quit)

. "$PSScriptRoot\common.ps1"
$build = if ($Release) { 'release' } else { 'debug' }
$exe = Join-Path (Get-ExeDir $build) 'tiny-skill-sync.exe'

function Stop-SandboxCopy {
    # Either build can close the copy that runs: it is found by its settings folder, not by its file.
    $any = 'release', 'debug' | ForEach-Object { Join-Path (Get-ExeDir $_) 'tiny-skill-sync.exe' } | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $any) { return $false }
    $p = Invoke-InSandbox { Start-Process $any -ArgumentList '--quit' -Wait -PassThru }
    return $p.ExitCode -eq 0
}

if ($Quit) {
    if (Stop-SandboxCopy) { 'The sandbox copy was closed.' } else { 'The sandbox copy was not running.' }
    return
}
if ($Fresh) {
    Stop-SandboxCopy | Out-Null
    if (Test-Path $Sandbox) { Remove-Item -Recurse -Force $Sandbox }
}
if ($Release) { & "$PSScriptRoot\build.ps1" | Out-Null } else { & "$PSScriptRoot\build.ps1" -DebugBuild | Out-Null }
Invoke-InSandbox {
    Start-Process $exe | Out-Null
    "Running $build build. Settings: $env:TSS_HOME  Skills: $env:TSS_SKILLS_DIR"
}
'Close it from its tray menu, or with: scripts\run.ps1 -Quit'
