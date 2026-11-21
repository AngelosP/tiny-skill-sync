# Windows PowerShell 5.1 / PowerShell 7. Needs copilot.exe and gh with fixture write access.
# Only this public fixture and two new e2e-*-spike-* refs are used. No transcript or raw CLI output.
$ErrorActionPreference = 'Stop'
$exe = $env:TSS_COPILOT_CLI
if ($null -eq $exe -and $env:LOCALAPPDATA) {
    $exe = Join-Path $env:LOCALAPPDATA 'GitHubCopilotCLI\copilot.exe'
}
if (-not $exe -or $exe -notmatch '^(?:[A-Za-z]:\\|\\\\[^.?\\][^\\]*\\[^\\]+\\)' -or $exe -notlike '*.exe' -or
    -not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    Write-Host 'SKIP: copilot.exe not found. Set TSS_COPILOT_CLI to its full path.'
    return
}
$exe = [IO.Path]::GetFullPath($exe)
. "$PSScriptRoot\..\scripts\common.ps1"

# Load the shared guards and ref helpers without initializing the live test's UI code.
$errors = $null
$lib = [Management.Automation.Language.Parser]::ParseFile("$PSScriptRoot\..\scripts\e2e-lib.ps1", [ref]$null, [ref]$errors)
$needed = @('Assert-LiveCopilotEnv', 'Assert-RunBranch', 'New-RunBranch', 'Remove-RunBranch')
$defs = @($lib.EndBlock.Statements | Where-Object { $_ -is [Management.Automation.Language.FunctionDefinitionAst] -and $_.Name -in $needed })
if ($errors.Count -or $defs.Count -ne $needed.Count) { throw 'Could not load the shared live-test helpers.' }
foreach ($def in $defs) { . ([scriptblock]::Create($def.Extent.Text)) }

$repo = 'AngelosP/tiny-skill-sync-fixtures-public'
$url = "https://github.com/$repo"
$folder = 'tools/spike-plugin'
$name = 'tss-s4-spike'
$market = 'tss-s4-market'
$skillPath = "skills/$name/SKILL.md"
$stamp = [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss', [Globalization.CultureInfo]::InvariantCulture)
$a, $b = "e2e-$stamp-spike-a", "e2e-$stamp-spike-b"
$branches = @($a, $b)
$TempRoot = [IO.Path]::GetFullPath($TempRoot).TrimEnd('\')
if ($TempRoot -notmatch '^[A-Za-z]:\\[^~]+$' -or -not $env:USERPROFILE -or -not $env:LOCALAPPDATA) {
    throw 'The probe needs a long-name drive path for TEMP and both profile paths.'
}
$root = Join-Path $TempRoot ('tss-e2e-spike-' + [guid]::NewGuid().ToString('N'))
foreach ($real in @((Join-Path (Get-LongPath $env:USERPROFILE) '.copilot'), (Join-Path (Get-LongPath $env:LOCALAPPDATA) 'copilot'))) {
    $real = [IO.Path]::GetFullPath($real).TrimEnd('\')
    if ("$root\".StartsWith("$real\", [StringComparison]::OrdinalIgnoreCase) -or
        "$real\".StartsWith("$root\", [StringComparison]::OrdinalIgnoreCase)) { throw 'TEMP overlaps a protected Copilot path.' }
}

function Invoke-SpikeChild($Start, [int]$Seconds, [string]$InputText = '') {
    $Start.UseShellExecute = $false
    $Start.CreateNoWindow = $true
    $Start.RedirectStandardInput = $true
    $Start.RedirectStandardOutput = $true
    $Start.RedirectStandardError = $true
    $Start.StandardOutputEncoding = [Text.Encoding]::UTF8
    $Start.StandardErrorEncoding = [Text.Encoding]::UTF8
    $p = [Diagnostics.Process]::Start($Start)
    try {
        $out = $p.StandardOutput.ReadToEndAsync()
        $err = $p.StandardError.ReadToEndAsync()
        if ($InputText) { $p.StandardInput.WriteLine($InputText) }
        $p.StandardInput.Close()
        if (-not $p.WaitForExit($Seconds * 1000) -or
            -not [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($out, $err), 5000)) { throw 'Child timed out.' }
        [pscustomobject]@{ Code = $p.ExitCode; Out = $out.Result; Err = $err.Result }
    } finally {
        if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue; [void]$p.WaitForExit(5000) }
        $p.Dispose()
    }
}

# Used by the imported ref helpers too. Stream GitHub request bodies through stdin, not a file.
function Invoke-Gh([string[]]$GhArgs, [string]$Body = '') {
    if ($GhArgs[0] -cne 'api') { throw 'Only the GitHub API is used.' }
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $ghExe
    $start.WorkingDirectory = Split-Path -Parent $ghExe
    $start.Arguments = ((@('api', '--hostname', 'github.com', '-H', 'Cache-Control: no-cache') +
        $GhArgs[1..($GhArgs.Length - 1)]) | ForEach-Object { Format-Arg $_ }) -join ' '
    $start.EnvironmentVariables.Remove('GH_DEBUG')
    $start.EnvironmentVariables['GH_PAGER'] = 'cat'
    $start.EnvironmentVariables['GH_NO_UPDATE_NOTIFIER'] = '1'
    $start.EnvironmentVariables['GH_NO_EXTENSION_UPDATE_NOTIFIER'] = '1'
    $start.EnvironmentVariables['GH_PROMPT_DISABLED'] = '1'
    $start.EnvironmentVariables['NoDefaultCurrentDirectoryInExePath'] = '1'
    $r = Invoke-SpikeChild $start 35 $Body
    if ($r.Code -ne 0) { throw 'GitHub fixture API request failed.' }
    $r.Out.Trim()
}

# A case keeps its fresh pair for subsequent list/update calls; no other case shares it.
function New-SpikeCase([string]$Id) {
    if ($Id -notmatch '^[a-z0-9-]+$') { throw 'Invalid case name.' }
    $dir = Join-Path $root $Id
    $e = @{ COPILOT_HOME = (Join-Path $dir 'home'); COPILOT_CACHE_HOME = (Join-Path $dir 'cache'); COPILOT_AUTO_UPDATE = 'false' }
    Assert-LiveCopilotEnv $root $e
    foreach ($path in @($dir, $e.COPILOT_HOME, $e.COPILOT_CACHE_HOME)) { New-Item -ItemType Directory -Path $path | Out-Null }
    [pscustomobject]@{ Dir = $dir; Home = $e.COPILOT_HOME; Cache = $e.COPILOT_CACHE_HOME; Env = $e }
}

function Invoke-SpikeCopilot($Case, [string[]]$Arguments, [int]$Seconds = 120) {
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $exe
    $start.WorkingDirectory = $Case.Dir
    $start.Arguments = ($Arguments | ForEach-Object { Format-Arg $_ }) -join ' '
    $e = $start.EnvironmentVariables
    foreach ($key in @($e.Keys)) {
        if ($key -match '^(COPILOT_|GH_|GITHUB_|GIT_|GCM_|XDG_|NODE_|TSS_|SSH_)') { $e.Remove($key) }
    }
    foreach ($key in $Case.Env.Keys) { $e[$key] = $Case.Env[$key] }
    foreach ($key in 'HOME', 'USERPROFILE', 'LOCALAPPDATA', 'APPDATA') { $e[$key] = $Case.Dir }
    foreach ($key in 'TEMP', 'TMP', 'TMPDIR') { $e[$key] = $Case.Cache }
    $e['GH_CONFIG_DIR'] = Join-Path $Case.Cache 'gh'
    $e['GH_PROMPT_DISABLED'] = '1'
    $e['GIT_CONFIG_NOSYSTEM'] = '1'
    $e['GIT_CONFIG_GLOBAL'] = 'NUL'
    $e['GIT_TERMINAL_PROMPT'] = '0'
    $e['GCM_INTERACTIVE'] = 'Never'
    $e['NO_COLOR'] = '1'
    $e['NoDefaultCurrentDirectoryInExePath'] = '1'
    Assert-LiveCopilotEnv $root $e
    if ($e['COPILOT_HOME'] -cne (Join-Path $Case.Dir 'home') -or $e['COPILOT_CACHE_HOME'] -cne (Join-Path $Case.Dir 'cache') -or
        "$($e['COPILOT_HOME'])$($e['COPILOT_CACHE_HOME'])" -match '~' -or
        -not (Test-Path -LiteralPath $e['COPILOT_HOME'] -PathType Container) -or
        -not (Test-Path -LiteralPath $e['COPILOT_CACHE_HOME'] -PathType Container)) { throw 'Copilot isolation check failed.' }
    Invoke-SpikeChild $start $Seconds
}

$public = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
$atoms = @($repo.Split('/') + @('https', 'http', 'git', 'file', 'github.com', 'github', 'GitHub', 'Git URL', 'repo', 'ref', 'path', 'branch',
    'tree', 'refs', 'heads', 'url', '.', ($repo.Split('/')[1] + '.git'), 'main', 'tools', 'spike-plugin', $a, $b, $name, $market,
    'skills', 'SKILL.md', 'plugin.json', 'installed-plugins.json', 'plugins.json',
    'installed-plugins', 'plugins', 'marketplaces', 'installed', '_direct', '_standalone', 'direct', 'standalone', 'local', 'builtin',
    '.copilot-plugin', '.claude-plugin', '.plugin', 'probe', 'local-control', 'cache', 'settings.json', 'config.json'))
foreach ($atom in $atoms) { [void]$public.Add($atom) }
$fields = @('name', 'marketplace', 'version', 'source', 'installedFrom', 'enabled', 'repo', 'repository', 'owner', 'url', 'gitUrl',
    'type', 'kind', 'ref', 'gitRef', 'branch', 'path', 'subdir', 'subdirectory', 'pluginPath', 'sourceUrl', 'sourcePath', 'installPath',
    'localPath', 'commit', 'sha', 'commitSha',
    'installedPlugins', 'plugins', 'enabledPlugins', 'extraKnownMarketplaces', 'sourceType', $name, $market, '_direct',
    "$name@_direct", "$name@$market", "${name}@")

function Get-PublicValue($Value, [int]$Depth = 0) {
    # Never relay unknown strings, keys, or diagnostics from CLI/config files.
    if ($Depth -ge 8) { return '<depth limit>' }
    if ($null -eq $Value -or $Value -is [bool]) { return $Value }
    if ($Value -is [string]) {
        if ($Value.Length -gt 512 -or $Value -match '://[^/?#]*@|(?i)bearer|(?:password|token)=') { return '<withheld>' }
        $relative = $Value -replace [regex]::Escape($root), 'probe'
        return [regex]::Replace($relative, '[^/\\:#@?&=, ]+', [Text.RegularExpressions.MatchEvaluator]{ param($m)
            if ($public.Contains($m.Value)) { $m.Value } else { '<other>' }
        })
    }
    if ($Value -is [array]) {
        $items = @(foreach ($v in $Value) { Get-PublicValue $v ($Depth + 1) })
        return ,$items
    }
    $result = [ordered]@{}
    foreach ($field in $fields) {
        $prop = $Value.PSObject.Properties[$field]
        if ($prop) { $result[$field] = Get-PublicValue $prop.Value ($Depth + 1) }
    }
    if (@($Value.PSObject.Properties).Count -and $result.Count -eq 0) { return '<unrecognized shape>' }
    $result
}

function Read-SpikeJson($File) {
    if ($File.Length -gt 4MB) { return }
    try { [IO.File]::ReadAllText($File.FullName) | ConvertFrom-Json } catch { }
}

function Get-SpikeFiles([string]$Dir) {
    foreach ($entry in Get-ChildItem -LiteralPath $Dir -Force) {
        if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
        if ($entry.PSIsContainer -and $entry.Name -ne '.git') { Get-SpikeFiles $entry.FullName }
        if (-not $entry.PSIsContainer) { $entry }
    }
}

$fixtures = @{}
function New-SpikeBuild([string]$Ref, [int]$Generation, [switch]$WithMarket) {
    $letter = $Ref.Substring($Ref.Length - 1)
    $variant = if ($Ref -ceq $a) { 1 } else { 2 }
    foreach ($area in 'folder', 'root') {
        $number = if ($area -eq 'folder') { 0 } else { 1 }
        $version = "$Generation.$variant.$number"
        $base = if ($area -eq 'folder') { "$folder/" } else { '' }
        $manifest = @{ name = $name; version = $version; description = 'Inert S4 fixture'; author = @{ name = 'TSS Fixture' } } | ConvertTo-Json -Compress
        $skill = "---`nname: $name`ndescription: Inert S4 fixture`n---`nS4 $stamp $letter $area generation $Generation.`n"
        $fixtures[$version] = [pscustomobject]@{ Ref = $Ref; Area = $area; Generation = $Generation; Skill = $skill; Manifest = $manifest }
        [void]$public.Add($version)
        @{ path = "${base}plugin.json"; mode = '100644'; type = 'blob'; content = $manifest }
        @{ path = "${base}${skillPath}"; mode = '100644'; type = 'blob'; content = $skill }
    }
    if ($WithMarket) {
        $catalog = @{ name = $market; owner = @{ name = 'TSS Fixture' }; plugins = @(@{ name = $name; source = "./$folder" }) }
        @{ path = 'marketplace.json'; mode = '100644'; type = 'blob'; content = ($catalog | ConvertTo-Json -Depth 6 -Compress) }
    }
}

function Test-SpikeRef([string]$Ref) {
    if ($Ref -cnotin $branches) { throw 'Ref outside the run.' }
    $refs = (Invoke-Gh @('api', "repos/$repo/git/matching-refs/heads/$Ref", '--jq', '.[].ref')) -split '\r?\n'
    $refs -ccontains "refs/heads/$Ref"
}

function Publish-SpikeBuild([string]$Ref, [int]$Generation, [switch]$WithMarket) {
    if ($Ref -cnotin $owned) { throw 'Ref was not created by this run.' }
    $entries = @(New-SpikeBuild $Ref $Generation -WithMarket:$WithMarket)
    $head = Invoke-Gh @('api', "repos/$repo/git/ref/heads/$Ref", '--jq', '.object.sha')
    # No base_tree: neither fixture carries files from main. Do not force the ref update.
    $tree = Invoke-Gh @('api', '-X', 'POST', "repos/$repo/git/trees", '--input', '-', '--jq', '.sha') (
        @{ tree = $entries } | ConvertTo-Json -Depth 7 -Compress)
    if ($head -cnotmatch '^[0-9a-f]{40}$' -or $tree -cnotmatch '^[0-9a-f]{40}$') { throw 'Missing fixture parent or tree SHA.' }
    $message = "Inert S4 plugin fixture`n`nCo-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
    $commit = Invoke-Gh @('api', '-X', 'POST', "repos/$repo/git/commits", '--input', '-', '--jq', '.sha') (
        @{ message = $message; tree = $tree; parents = @($head) } | ConvertTo-Json -Compress)
    if ($commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Missing fixture commit SHA.' }
    [void]$public.Add($commit)
    [void](Invoke-Gh @('api', '-X', 'PATCH', "repos/$repo/git/refs/heads/$Ref", '--input', '-') (
        @{ sha = $commit; force = $false } | ConvertTo-Json -Compress))

    $proof = if ($WithMarket) { 'marketplace.json' } else { "$folder/$skillPath" }
    $want = ($entries | Where-Object { $_.path -ceq $proof }).content
    for ($i = 0; $i -lt 9; $i++) {
        try {
            $raw = Invoke-WebRequest -UseBasicParsing -TimeoutSec 10 -Uri "https://raw.githubusercontent.com/$repo/$Ref/${proof}?s4=$commit"
            if ([Text.Encoding]::UTF8.GetString($raw.RawContentStream.ToArray()) -ceq $want) { return }
        } catch { }
        Start-Sleep -Seconds 3
    }
    throw 'The public fixture content did not reach the new commit.'
}

function Show-SpikeSnapshot($Case) {
    $r = Invoke-SpikeCopilot $Case @('plugin', 'list', '--json') 30
    $rows = @()
    $rowsRead = $false
    if ($r.Code -eq 0) {
        try {
            $parsed = ConvertFrom-Json -InputObject $r.Out
            if ($r.Out.TrimStart().StartsWith('[')) { $rows = @($parsed | Where-Object { $null -ne $_ }); $rowsRead = $true }
            elseif ($parsed.PSObject.Properties['plugins']) { $rows = @($parsed.plugins); $rowsRead = $true }
        } catch { }
    }
    $rows = @($rows | Where-Object { $_.source -ne 'builtin' })
    Write-Host "  list exit $($r.Code); readable=$rowsRead; non-builtin rows=$($rows.Count)"
    foreach ($row in $rows) {
        $details = [ordered]@{}
        foreach ($field in 'name', 'source', 'installedFrom', 'version', 'marketplace', 'enabled') {
            $details[$field] = if ($row.PSObject.Properties[$field]) { Get-PublicValue $row.$field } else { '<absent>' }
        }
        Write-Host ('  row ' + ($details | ConvertTo-Json -Compress -Depth 12))
    }
    $builds = @(foreach ($file in Get-SpikeFiles $Case.Home) {
        $rel = $file.FullName.Substring($Case.Home.Length + 1).Replace('\', '/')
        if ($rel -in 'settings.json', 'config.json', 'installed-plugins.json', 'plugins.json') {
            $state = Read-SpikeJson $file
            if ($null -eq $state) { Write-Host "  $rel present; not readable JSON"; continue }
            $data = [ordered]@{}
            foreach ($key in 'installedPlugins', 'plugins', 'enabledPlugins', 'extraKnownMarketplaces') {
                if ($state.PSObject.Properties[$key]) { $data[$key] = Get-PublicValue $state.$key }
            }
            Write-Host ('  ' + $rel + ' ' + ($data | ConvertTo-Json -Compress -Depth 12))
        }
        if ($rel -notmatch '^installed-plugins/([^/]+)/(?:[^/]+/)*plugin\.json$') { continue }
        $namespace = $Matches[1]
        $manifest = Read-SpikeJson $file
        if ($manifest.name -cne $name) { continue }
        $entry = $file.Directory
        if ($entry.Name -in '.plugin', '.copilot-plugin', '.claude-plugin') { $entry = $entry.Parent }
        $nested = $rel -like "installed-plugins/*/$folder/*"
        $skill = Get-SpikeFiles $entry.FullName | Where-Object { $_.FullName.Substring($entry.FullName.Length + 1).Replace('\', '/') -ceq $skillPath } | Select-Object -First 1
        $build = $fixtures[[string]$manifest.version]
        $exact = $build -and $skill -and $skill.Length -lt 4KB -and
            ([IO.File]::ReadAllText($skill.FullName).Replace("`r`n", "`n") -ceq $build.Skill)
        $where = Get-PublicValue $rel
        if (-not $exact -or $nested) { Write-Host "  disk $where (not top-level fixture proof)"; continue }
        $registered = @($rows | Where-Object {
            $_.name -ceq $name -and $_.version -ceq $manifest.version -and
                ($_.marketplace -ceq $namespace -or (-not $_.marketplace -and $namespace -in '_direct', '_standalone'))
        }).Count -eq 1
        Write-Host "  disk $where matches $($build.Ref) $($build.Area) generation $($build.Generation), version $($manifest.version); list match=$registered"
        [pscustomobject]@{ Ref = $build.Ref; Area = $build.Area; Generation = $build.Generation; Registered = $registered }
    })
    if (-not $builds.Count) { Write-Host '  no matching installed manifest and skill marker' }
    $builds
}

function Invoke-SpikeChange($Case, [string[]]$Arguments, [string]$Label) {
    $r = Invoke-SpikeCopilot $Case $Arguments
    $diagnostic = $r.Err + "`n" + $r.Out
    $kind = switch -Regex ($diagnostic) {
        'rate.limit' { 'rate limit'; break }
        'already installed|ambiguous|conflict' { 'name conflict or already installed'; break }
        'permission denied|authenticat|not logged in' { 'authentication or permissions'; break }
        'failed to clone|clone failed|repository not found|remote ref' { 'git or ref diagnostic'; break }
        'not found|does not exist|not a valid|invalid' { 'not found or invalid'; break }
        default { 'no classified diagnostic' }
    }
    Write-Host "$Label : exit $($r.Code); $kind"
    Show-SpikeSnapshot $Case
}

function Remove-SpikeTree([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        if ($item.PSIsContainer) { [IO.Directory]::Delete($Path) } else { [IO.File]::Delete($Path) }
    } elseif ($item.PSIsContainer) {
        foreach ($child in Get-ChildItem -LiteralPath $Path -Force) { Remove-SpikeTree $child.FullName }
        [IO.Directory]::Delete($Path)
    } else { Remove-Item -LiteralPath $Path -Force }
}

$owned = New-Object 'Collections.Generic.List[string]'
$attempted = New-Object 'Collections.Generic.List[string]'
$rootOwned = $false
$failed = $false
$phase = 'isolated version'
$check = @{ COPILOT_HOME = (Join-Path $root 'check-home'); COPILOT_CACHE_HOME = (Join-Path $root 'check-cache'); COPILOT_AUTO_UPDATE = 'false' }
try {
    Assert-LiveCopilotEnv $root $check
    New-Item -ItemType Directory -Path $root | Out-Null
    $rootOwned = $true
    $r = Invoke-SpikeCopilot (New-SpikeCase 'version') @('--version') 30
    $v = [regex]::Match($r.Out + "`n" + $r.Err, '(?im)^(?:GitHub Copilot CLI\s+|copilot\s+)?(?:version\s+|v)?([0-9]+\.[0-9]+\.[0-9]+(?:-(?:alpha|beta|preview|rc)(?:[.][0-9]+)?)?)\.?(?=\s|$)')
    if ($r.Code -ne 0 -or -not $v.Success) { throw 'Version unavailable.' }
    Write-Host "S4 $stamp; isolated copilot --version: $($v.Groups[1].Value)"

    $phase = 'fixture preflight'
    $ghExe = (Get-Command gh.exe -CommandType Application | Select-Object -First 1).Source
    $info = Invoke-Gh @('api', "repos/$repo") | ConvertFrom-Json
    $main = Invoke-Gh @('api', "repos/$repo/git/trees/main?recursive=1") | ConvertFrom-Json
    if ($info.default_branch -cne 'main' -or $info.private -ne $false -or $main.truncated -ne $false -or
        @($main.tree | Where-Object { $_.path -ceq $folder -or $_.path -clike "$folder/*" }).Count) {
        throw 'Public main must not contain the subfolder control.'
    }
    Write-Host "Default branch main has no $folder (negative control; main is never changed)."
    foreach ($ref in $branches) { if (Test-SpikeRef $ref) { throw 'Ref name already in use.' } }
    foreach ($ref in $branches) {
        $phase = "create $ref"
        $attempted.Add($ref)
        New-RunBranch $repo 'main' $ref
        $owned.Add($ref)
        $phase = "publish $ref generation 1"
        Publish-SpikeBuild $ref 1
    }

    $forms = @(
        "${repo}:${folder}", "${repo}:${folder}#{0}", "${repo}#{0}:${folder}", "${repo}@{0}:${folder}",
        "$url.git#{0}", "$url.git#{0}:${folder}", "$url.git:${folder}#{0}", "$url/tree/{0}/$folder",
        "${repo}:${folder}@{0}", "$url.git#{0}/$folder", "${repo}#{0}", "$url.git?ref={0}&path=$folder"
    )
    $updates = New-Object 'Collections.Generic.List[object]'
    $bFolderSpec = $null
    for ($i = 0; $i -lt $forms.Count; $i++) {
        $spec = $forms[$i] -f $a
        $phase = "install $spec"
        $case = New-SpikeCase "a-$i"
        $got = @(Invoke-SpikeChange $case @('plugin', 'install', $spec) $phase)
        $verified = @($got | Where-Object { $_.Registered -and $_.Ref -ceq $a })
        if (@($got | Where-Object Registered).Count) { $updates.Add(@{ Case = $case; Spec = $spec }) }
        if ($i -eq 0 -or -not $verified.Count) { continue }
        $spec = $forms[$i] -f $b
        $phase = "install $spec"
        $case = New-SpikeCase "b-$i"
        $got = @(Invoke-SpikeChange $case @('plugin', 'install', $spec) $phase)
        $verified = @($got | Where-Object { $_.Registered -and $_.Ref -ceq $b })
        if ($verified.Count) { $updates.Add(@{ Case = $case; Spec = $spec }) }
        if (-not $bFolderSpec -and @($verified | Where-Object { $_.Area -eq 'folder' }).Count) { $bFolderSpec = $spec }
    }

    $phase = 'standalone updates'
    if ($updates.Count) {
        foreach ($ref in $branches) { Publish-SpikeBuild $ref 2 }
        foreach ($install in $updates) {
            $phase = "update $name after $($install.Spec)"
            [void](Invoke-SpikeChange $install.Case @('plugin', 'update', $name) $phase)
        }
    } else { Write-Host 'Update untested: no list-and-disk-verified GitHub standalone install.' }

    $phase = 'marketplace name clash'
    Publish-SpikeBuild $b 2 -WithMarket
    # Fall back to the same inert bytes locally if no GitHub subfolder form was proven.
    $direct = $bFolderSpec
    $directLabel = $direct
    if (-not $direct) {
        $local = Join-Path $root 'local-control'
        $localSkill = Join-Path $local ($skillPath.Replace('/', '\'))
        [void][IO.Directory]::CreateDirectory((Split-Path -Parent $localSkill))
        [IO.File]::WriteAllText((Join-Path $local 'plugin.json'), $fixtures['2.2.0'].Manifest)
        [IO.File]::WriteAllText($localSkill, $fixtures['2.2.0'].Skill)
        $direct = $local
        $directLabel = 'local control with the same branch B bytes (not a GitHub result)'
    }
    foreach ($first in 'direct', 'market') {
        $case = New-SpikeCase "clash-$first"
        $marketInstall = @('plugin', 'install', "$name@$market")
        $directInstall = @('plugin', 'install', $direct)
        if ($first -eq 'direct') { [void](Invoke-SpikeChange $case $directInstall "direct first: $directLabel") }
        [void](Invoke-SpikeChange $case @('plugin', 'marketplace', 'add', "$repo#$b") 'register branch B marketplace')
        [void](Invoke-SpikeChange $case $marketInstall "install $name@$market")
        if ($first -eq 'market') { [void](Invoke-SpikeChange $case $directInstall "market first: $directLabel") }
    }
} catch {
    # The exception can contain arbitrary native/API/file data. Only print the known phase.
    Write-Host "BLOCKED during $phase (native and API diagnostics are not printed)."
    $failed = $true
} finally {
    foreach ($ref in $attempted) {
        $gone = $false
        for ($i = 0; $i -lt 3 -and -not $gone; $i++) {
            if ($ref -cin $owned) { try { Remove-RunBranch $repo $ref } catch { } }
            try { $gone = -not (Test-SpikeRef $ref) } catch { }
            if (-not $gone) { Start-Sleep -Seconds 2 }
        }
        if ($gone) { Write-Host "Cleanup: verified $ref absent." }
        else { Write-Host "Cleanup: could not verify $ref absent (an unconfirmed ref is never deleted)."; $failed = $true }
    }
    if ($rootOwned) {
        for ($i = 0; $i -lt 3 -and (Test-Path -LiteralPath $root); $i++) {
            try { Assert-LiveCopilotEnv $root $check; Remove-SpikeTree $root } catch { Start-Sleep -Seconds 1 }
        }
        if (Test-Path -LiteralPath $root) { Write-Host 'Cleanup: throwaway root still exists.'; $failed = $true }
        else { Write-Host 'Cleanup: throwaway root absent.' }
    }
}
if ($failed) { exit 1 }
