<#
  Spike S2a - how GitHub answers conditional polling and downloads. READ-ONLY requests only.

    .\s2-github-probe.ps1 -Owner anthropics -Repo skills -Branch main -Path skills
    .\s2-github-probe.ps1 -Owner AngelosP -Repo tiny-skill-sync-fixtures-private -Auth token

  -Auth token takes the token from `gh auth token` into memory only. It is never printed or written.
  Anonymous REST access is limited to 60 requests/hour per IP, so this script keeps its request count small
  and reads /rate_limit (which is free) between steps to see exactly what each request cost.
#>
param(
    [string]$Owner = 'anthropics', [string]$Repo = 'skills', [string]$Branch = 'main', [string]$Path = 'skills',
    [ValidateSet('none', 'token')] [string]$Auth = 'none',
    [string[]]$Sections = @('1', '2', '3', '4', '5')
)
$ErrorActionPreference = 'Stop'
$Sections = @($Sections | ForEach-Object { $_ -split ',' })
$script:Token = $null
if ($Auth -eq 'token') { $script:Token = (& gh auth token).Trim() }

function Call([string]$Url, [string[]]$Headers = @(), [switch]$NoAuth) {
    $a = @('-sS', '-i', '--max-time', '30', '-w', "`n@@time=%{time_total};bytes=%{size_download}", '-H', 'User-Agent: tiny-skill-sync-spike')
    if ($script:Token -and -not $NoAuth) { $a += @('-H', ('Authorization: Bearer ' + $script:Token)) }
    # Windows PowerShell 5.1 drops embedded double quotes when starting a native program (ETags contain them), so escape them.
    $legacy = $PSVersionTable.PSVersion -lt [version]'7.3'
    foreach ($hd in $Headers) { $a += @('-H', $(if ($legacy) { $hd.Replace('"', '\"') } else { $hd })) }
    $a += $Url
    $raw = (& curl.exe @a 2>&1 | Out-String)
    $trailer = [regex]::Match($raw, '@@time=([\d.]+);bytes=(\d+)\s*$')
    $raw = $raw.Substring(0, $trailer.Index)
    $split = $raw.IndexOf("`r`n`r`n")
    $head = if ($split -ge 0) { $raw.Substring(0, $split) } else { $raw }
    $body = if ($split -ge 0) { $raw.Substring($split + 4) } else { '' }
    $lines = $head -split "`r`n"
    $h = @{}
    foreach ($l in $lines | Select-Object -Skip 1) { $i = $l.IndexOf(':'); if ($i -gt 0) { $h[$l.Substring(0, $i).Trim().ToLowerInvariant()] = $l.Substring($i + 1).Trim() } }
    [pscustomobject]@{ Status = ($lines[0] -replace '^HTTP/\S+\s+', ''); H = $h; Body = $body; Ms = [int]([double]$trailer.Groups[1].Value * 1000); Bytes = [int]$trailer.Groups[2].Value }
}

function Rl($r) {
    if ($r.H.ContainsKey('x-ratelimit-remaining')) { '{0}/{1} ({2})' -f $r.H['x-ratelimit-remaining'], $r.H['x-ratelimit-limit'], $r.H['x-ratelimit-resource'] } else { 'no ratelimit headers' }
}
function Show([string]$label, $r, [string]$extra = '') {
    '[{0,-34}] {1,-22} etag={2} cache-control={3} rl={4} body={5}B {6}ms {7}' -f $label, $r.Status, $r.H['etag'], $r.H['cache-control'], (Rl $r), $r.Bytes, $r.Ms, $extra
}
function CoreRemaining() {
    $r = Call 'https://api.github.com/rate_limit'
    $j = $r.Body | ConvertFrom-Json
    [pscustomobject]@{ Remaining = $j.resources.core.remaining; Limit = $j.resources.core.limit; Reset = $j.resources.core.reset }
}

function Want([string]$n) { $Sections -contains $n }
$api = "https://api.github.com/repos/$Owner/$Repo"
"=== S2a: $Owner/$Repo branch=$Branch path=$Path auth=$Auth"
$c0 = CoreRemaining
"core quota at start: $($c0.Remaining)/$($c0.Limit) (querying /rate_limit is free)"

$tj = $null
if (Want '1') {
"`n--- 1. Conditional request on the tree of the skills root (one level, via 'branch:path')"
$t1 = Call "$api/git/trees/${Branch}:${Path}"
$tj = $null; if ($t1.Status -like '200*') { $tj = $t1.Body | ConvertFrom-Json }
Show 'trees/branch:path' $t1 $(if ($tj) { "tree.sha=$($tj.sha) entries=$($tj.tree.Count) truncated=$($tj.truncated)" })
$c1 = CoreRemaining; "core quota after request 1: $($c1.Remaining) (cost $($c0.Remaining - $c1.Remaining))"
if ($t1.H['etag']) {
    $t2 = Call "$api/git/trees/${Branch}:${Path}" @("If-None-Match: $($t1.H['etag'])")
    Show 'trees/branch:path + If-None-Match' $t2
    $c2 = CoreRemaining; "core quota after the conditional request: $($c2.Remaining) (cost $($c1.Remaining - $c2.Remaining))  -> a 304 is $(if ($c1.Remaining -eq $c2.Remaining) { 'FREE' } else { 'COUNTED' }) in this auth mode"
}

}

if (Want '2') {
"`n--- 2. Tracked ref as a bare commit SHA (media type application/vnd.github.sha)"
$s1 = Call "$api/commits/$Branch" @('Accept: application/vnd.github.sha')
Show 'commits/branch (sha media type)' $s1 ("sha=" + $s1.Body.Trim())
if ($s1.H['etag']) { Show 'same + If-None-Match' (Call "$api/commits/$Branch" @('Accept: application/vnd.github.sha', "If-None-Match: $($s1.H['etag'])")) }

}

if (Want '3') {
"`n--- 3. Alternatives for the same question"
$ref = Call "$api/git/ref/heads/$Branch"; Show 'git/ref/heads/branch' $ref
$con = Call "$api/contents/${Path}?ref=$Branch"; Show 'contents/path?ref=branch' $con
$repoInfo = Call $api; $rj = if ($repoInfo.Status -like '200*') { $repoInfo.Body | ConvertFrom-Json } else { $null }
Show 'repo (default_branch lookup)' $repoInfo $(if ($rj) { "default_branch=$($rj.default_branch) private=$($rj.private)" })

}

if ((Want '4') -and $tj) {
    "`n--- 4. Raw download of one file in the skills root (not the REST API)"
    $dir = $tj.tree | Where-Object { $_.type -eq 'tree' } | Select-Object -First 1
    if ($dir) {
        $sub = Call "$api/git/trees/$($dir.sha)?recursive=1"
        $sj = $sub.Body | ConvertFrom-Json
        Show "trees/<$($dir.path)>?recursive=1" $sub "entries=$($sj.tree.Count) truncated=$($sj.truncated)"
        $file = $sj.tree | Where-Object { $_.type -eq 'blob' } | Select-Object -First 1
        $rawUrl = "https://raw.githubusercontent.com/$Owner/$Repo/$Branch/$Path/$($dir.path)/$($file.path)"
        $r1 = Call $rawUrl
        Show 'raw.githubusercontent.com' $r1 "file=$($dir.path)/$($file.path) expectedSize=$($file.size) blobSha=$($file.sha)"
        if ($r1.H['etag']) { Show 'raw + If-None-Match' (Call $rawUrl @("If-None-Match: $($r1.H['etag'])")) }
        $blob = Call "$api/git/blobs/$($file.sha)" @('Accept: application/vnd.github.raw+json')
        Show 'git/blobs/sha (raw media type)' $blob
    }
}

if (Want '5') {
"`n--- 5. Error shapes (never to be read as 'upstream deleted')"
Show 'nonexistent repo' (Call "https://api.github.com/repos/$Owner/this-repo-does-not-exist-tss/git/trees/${Branch}")
Show 'missing path in existing repo' (Call "$api/git/trees/${Branch}:no-such-folder-tss")
Show 'missing branch' (Call "$api/git/trees/no-such-branch-tss:${Path}")
Show 'bad credentials' (Call "$api/commits/$Branch" @('Authorization: Bearer ghp_invalidinvalidinvalidinvalidinvalid0000', 'Accept: application/vnd.github.sha') -NoAuth)
}

$cEnd = CoreRemaining
"`ncore quota at end: $($cEnd.Remaining)/$($cEnd.Limit); this script used $($c0.Remaining - $cEnd.Remaining) counted requests"
