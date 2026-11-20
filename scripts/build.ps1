<#
.SYNOPSIS
  Builds the tray app (tiny-skill-sync.exe) and the command line (tss.exe).

.DESCRIPTION
  Release by default: that is what users run and what is measured. -DebugBuild makes a quicker, unoptimized build.
  Prints the size of each executable (the budget for the tray app is 1.1 MiB).

.EXAMPLE
  scripts\build.ps1
  scripts\build.ps1 -DebugBuild
#>
param([switch]$DebugBuild)

. "$PSScriptRoot\common.ps1"
Use-Cargo
Push-Location $RepoRoot
try {
    if ($DebugBuild) {
        Invoke-Native cargo @('build', '--bins')
        $build = 'debug'
    } else {
        Invoke-Native cargo @('build', '--release', '--bins')
        $build = 'release'
    }
    foreach ($name in 'tiny-skill-sync.exe', 'tss.exe') {
        $exe = Get-Item (Join-Path (Get-ExeDir $build) $name)
        '{0,-20} {1,12:N0} bytes  ({2:N2} MiB)  {3}' -f $name, $exe.Length, ($exe.Length / 1MB), $exe.FullName
    }
} finally {
    Pop-Location
}
