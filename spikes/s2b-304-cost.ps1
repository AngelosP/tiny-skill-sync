<#
  Spike S2b - does an authenticated 304 really cost nothing?

  The same token is also used by gh, Copilot and other tools, so one before/after reading can be confounded by other
  processes. This repeats the conditional request many times and also samples the quota with NO requests of ours
  (a control), so background use cannot be mistaken for the cost of a 304.
  The token comes from `gh auth token` and stays in memory.
#>
param(
    [string]$Url = 'https://api.github.com/repos/AngelosP/tiny-skill-sync-fixtures-private/git/trees/main:skills',
    [int]$Count = 20
)
$ErrorActionPreference = 'Stop'
$tok = (& gh auth token).Trim()
$legacy = $PSVersionTable.PSVersion -lt [version]'7.3'

function Core() {
    $j = (& curl.exe -s -H "Authorization: Bearer $tok" -H 'User-Agent: tiny-skill-sync-spike' https://api.github.com/rate_limit) | ConvertFrom-Json
    [int]$j.resources.core.remaining
}
function Get-ETag() {
    $h = & curl.exe -s -D - -o NUL -H "Authorization: Bearer $tok" -H 'User-Agent: tiny-skill-sync-spike' $Url
    ($h | Where-Object { $_ -match '^etag:' } | Select-Object -First 1) -replace '^etag:\s*', ''
}

"url: $Url"
"control: quota with no requests of ours, sampled 6 times over 12 s"
$ctl = 1..6 | ForEach-Object { $q = Core; Start-Sleep -Seconds 2; $q }
"  samples: $($ctl -join ' ')  (drift = $($ctl[0] - $ctl[-1]) from other users of this token)"

$etag = Get-ETag
$cond = ('If-None-Match: ' + $etag); if ($legacy) { $cond = $cond.Replace('"', '\"') }
"etag: $etag"
$before = Core
$codes = 1..$Count | ForEach-Object { & curl.exe -s -o NUL -w '%{http_code}' -H "Authorization: Bearer $tok" -H 'User-Agent: tiny-skill-sync-spike' -H $cond $Url }
$after = Core
"sent $Count conditional requests; status codes: $(($codes | Group-Object | ForEach-Object { '{0} x{1}' -f $_.Name, $_.Count }) -join ', ')"
"quota before=$before after=$after  -> used $($before - $after) (the /rate_limit call itself is free; expect ~0 if 304s are free, ~$Count if counted)"
