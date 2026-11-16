<#
.SYNOPSIS
  Spike S3: builds the tray and floor examples in every profile / CRT variant that the measurements compare.
.DESCRIPTION
  Dynamic CRT  -> target\<profile>\examples\       (RUSTFLAGS=-C target-feature=-crt-static)
  Static CRT   -> target-crt\<profile>\examples\   (RUSTFLAGS=-C target-feature=+crt-static)
  The project default (.cargo\config.toml) is the static CRT; RUSTFLAGS overrides it, so both variants are explicit.
  Prints the exe size of every result.
#>
$ErrorActionPreference = 'Continue'
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Invoke-Build([string]$Profile, [string]$TargetDir, [string]$RustFlags) {
    $env:CARGO_TARGET_DIR = $TargetDir
    if ($RustFlags) { $env:RUSTFLAGS = $RustFlags } else { Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue }
    [string[]]$profileArgs = if ($Profile -eq 'release') { '--release' } else { '--profile', $Profile }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $out = cargo build @profileArgs --features spikes --example s3_tray --example s3_floor 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $sw.Stop()
    "{0,-8} {1,-11} build {2,5:N1}s exit={3}" -f $Profile, $TargetDir, $sw.Elapsed.TotalSeconds, $code
    if ($code -ne 0) { $out | Select-Object -Last 40; return }
    foreach ($name in 's3_tray', 's3_floor') {
        $exe = Join-Path $root "$TargetDir\$Profile\examples\$name.exe"
        if (Test-Path $exe) { "         {0,-9} {1,9:N0} bytes" -f $name, (Get-Item $exe).Length }
    }
}

try {
    foreach ($p in 'release', 'size-s', 'size-z') { Invoke-Build $p 'target' '-C target-feature=-crt-static' }
    foreach ($p in 'release', 'size-s', 'size-z') { Invoke-Build $p 'target-crt' '-C target-feature=+crt-static' }
}
finally {
    Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue
    Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue
}
