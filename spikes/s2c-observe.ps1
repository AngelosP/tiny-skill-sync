<#
  Spike S2c - observe a fixture repo before/after a mutation (token mode, read-only requests).

    .\s2c-observe.ps1 -Repo tiny-skill-sync-fixtures-public                       # prints commit sha, tree etag/sha, entries
    .\s2c-observe.ps1 -Repo ... -TreeEtag '"abc"' -CommitEtag '"def"'            # also sends If-None-Match and reports 304/200
    .\s2c-observe.ps1 -Repo ... -WaitForChange -TreeEtag '"abc"'                 # polls every 5 s until the tree response changes
#>
param(
    [string]$Owner = 'AngelosP', [string]$Repo = 'tiny-skill-sync-fixtures-public', [string]$Branch = 'main', [string]$Path = 'skills',
    [ValidateSet('none', 'token')][string]$Auth = 'token',
    [string]$TreeEtag, [string]$CommitEtag, [switch]$WaitForChange, [switch]$TreeOnly, [int]$MaxSeconds = 180, [int]$EverySeconds = 5
)
$ErrorActionPreference = 'Stop'
$tok = if ($Auth -eq 'token') { (& gh auth token).Trim() } else { $null }
$legacy = $PSVersionTable.PSVersion -lt [version]'7.3'
$api = "https://api.github.com/repos/$Owner/$Repo"

# Windows PowerShell 5.1 strips embedded double quotes from arguments passed through -File, so accept ETags with or without them.
function Fix-Etag([string]$e) {
    if (-not $e) { return $e }
    if ($e.StartsWith('W/')) { $b = $e.Substring(2).Trim('"'); return 'W/"' + $b + '"' }
    return '"' + $e.Trim('"') + '"'
}
$TreeEtag = Fix-Etag $TreeEtag; $CommitEtag = Fix-Etag $CommitEtag

function Get-Http([string]$Url, [string[]]$Headers = @()) {
    $a = @('-s', '-i', '--max-time', '30', '-H', 'User-Agent: tiny-skill-sync-spike')
    if ($tok) { $a += @('-H', "Authorization: Bearer $tok") }
    foreach ($hd in $Headers) { $a += @('-H', $(if ($legacy) { $hd.Replace('"', '\"') } else { $hd })) }
    $raw = (& curl.exe @a $Url | Out-String)
    $split = $raw.IndexOf("`r`n`r`n"); $head = if ($split -ge 0) { $raw.Substring(0, $split) } else { $raw }; $body = if ($split -ge 0) { $raw.Substring($split + 4) } else { '' }
    $lines = $head -split "`r`n"; $h = @{}
    foreach ($l in $lines | Select-Object -Skip 1) { $i = $l.IndexOf(':'); if ($i -gt 0) { $h[$l.Substring(0, $i).Trim().ToLowerInvariant()] = $l.Substring($i + 1).Trim() } }
    [pscustomobject]@{ Code = [int](($lines[0] -split ' ')[1]); H = $h; Body = $body }
}

function Observe() {
    if ($TreeOnly) { $c = [pscustomobject]@{ Code = 0; H = @{}; Body = '' } }
    else {
        $hdr = @('Accept: application/vnd.github.sha'); if ($CommitEtag) { $hdr += "If-None-Match: $CommitEtag" }
        $c = Get-Http "$api/commits/$Branch" $hdr
    }
    $hdr = @(); if ($TreeEtag) { $hdr += "If-None-Match: $TreeEtag" }
    $t = Get-Http "$api/git/trees/${Branch}:${Path}" $hdr
    $out = [ordered]@{ Commit = $c; Tree = $t }
    $out
}

function Describe($o) {
    $c = $o.Commit; $t = $o.Tree
    if (-not $TreeOnly) { 'commit: HTTP {0} sha={1} etag={2}' -f $c.Code, $(if ($c.Code -eq 200) { $c.Body.Trim() } else { '(not modified)' }), $c.H['etag'] }
    if ($t.Code -eq 200) {
        $j = $t.Body | ConvertFrom-Json
        'tree  : HTTP 200 etag={0} tree.sha={1} entries: {2}' -f $t.H['etag'], $j.sha, (($j.tree | ForEach-Object { $_.path + '=' + $_.sha.Substring(0, 7) }) -join ' ')
    }
    else { 'tree  : HTTP {0} etag={1} {2}' -f $t.Code, $t.H['etag'], $(if ($t.Code -ge 400) { $t.Body.Trim() }) }
}

if ($WaitForChange) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    do {
        $o = Observe
        '[{0}] +{1,5:N1}s auth={2} commit HTTP {3}  tree HTTP {4}' -f (Get-Date -Format 'HH:mm:ss.fff'), $sw.Elapsed.TotalSeconds, $Auth, $o.Commit.Code, $o.Tree.Code
        if ($o.Tree.Code -ne 304) { break }
        Start-Sleep -Seconds $EverySeconds
    } while ($sw.Elapsed.TotalSeconds -lt $MaxSeconds)
    "waited {0:N1} s until the tree response left 304 (HTTP {1})" -f $sw.Elapsed.TotalSeconds, $o.Tree.Code
    Describe $o
}
else { Describe (Observe) }
