<#
.SYNOPSIS
  Make the README GIF from the real app and disposable public fixture data.
.DESCRIPTION
  Needs a signed-in gh with push access to AngelosP/tiny-skill-sync-fixtures-public,
  and Python with scripts\requirements-demo.txt installed. Takes about six minutes:
  the automatic-update scene really waits for a five-minute scheduled check.
  Only the app's own windows are captured (PrintWindow), never the desktop.
  Raw frames stay in bench\out for review; dev.ps1 clean removes them.
.PARAMETER Python
  Python executable with Pillow installed. No packages are installed by this script.
.PARAMETER SkipBuild
  Use the existing release exes.
#>
param([string]$Python = 'python', [switch]$SkipBuild)

. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\e2e-lib.ps1"

$fixture = 'AngelosP/tiny-skill-sync-fixtures-public'
$branch = 'e2e-demo-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$root = $null
$tray = $null
$createdBranch = $false
$failed = $false
$savedGhHost = $env:GH_HOST
$env:GH_HOST = 'github.com'
$utf8 = New-Object Text.UTF8Encoding($false)
$shots = Join-Path $RepoRoot ("bench\out\demo-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$scenes = New-Object System.Collections.Generic.List[object]

function Assert-Demo([bool]$Ok, [string]$What) {
    if (-not $Ok) { throw $What }
    Write-Host "PASS: $What"
}

function Wait-PublicDemo([string]$Commit, [string]$Relative, [string]$Blob) {
    [void](Wait-Live {
        (Get-PublicCommit $fixture $branch) -eq $Commit -and
        (Get-PublicFiles $fixture $branch 'demo-skills')[$Relative] -eq $Blob
    } 'the public commit and listing to agree' 150 15000)
}

function Save-DemoScene([IntPtr]$Window, [string]$Name, [string]$Heading, [string]$Caption,
                        [IntPtr]$Focus, [string]$Footer, [int]$Duration = 4000) {
    Assert-Demo ([TssLive]::Ping($Window, 2000) -ge 0) 'the demo window responds'
    [void](Wait-Live { -not [TssLive]::Pending($Window) } 'the window to finish painting' 10 100)
    Start-Sleep -Milliseconds 400
    $bounds = @([TssLive]::Bounds($Window) -split ',' | ForEach-Object { [int]$_ })
    $parts = @(foreach ($panel in @(Get-Layout $Window | Where-Object Shown)) {
        Get-Layout $panel.Handle | Where-Object { $_.Shown -and [TssLive]::Text($_.Handle) }
    })
    foreach ($part in $parts) {
        $text = [TssLive]::Text($part.Handle)
        if ($text -match '[A-Za-z]:\\|\\\\|gh[pousr]_[A-Za-z0-9]|github_pat_') {
            throw 'A demo control contains a local path or credential-shaped text; refusing the capture.'
        }
    }
    $path = Join-Path $shots "$Name.png"
    # PrintWindow sometimes leaves a blank lower band. Require every visible text control to have painted;
    # never fall back to a desktop capture that might include another person's window.
    [void](Wait-Live {
        [void][TssLive]::Capture($Window, $path)
        $painted = $true
        foreach ($part in $parts) {
            $b = @([TssLive]::Bounds($part.Handle) -split ',' | ForEach-Object { [int]$_ })
            if ($b[0] -lt $bounds[0] -or $b[1] -lt $bounds[1] -or $b[2] -gt $bounds[2] -or $b[3] -gt $bounds[3]) { continue }
            if ([TssLive]::Contrast($path, $b[0] - $bounds[0], $b[1] - $bounds[1], $b[2] - $b[0], $b[3] - $b[1]) -lt 24) {
                [void][TssLive]::Repaint($part.Handle)
                $painted = $false
            }
        }
        $painted
    } 'a complete window-only capture' 15 300)
    $b = @([TssLive]::Bounds($Focus) -split ',' | ForEach-Object { [int]$_ })
    $scenes.Add([ordered]@{
        shot = "$Name.png"; heading = $Heading; caption = $Caption; footer = $Footer; duration = $Duration
        focus = @(($b[0] - $bounds[0]), ($b[1] - $bounds[1]), ($b[2] - $bounds[0]), ($b[3] - $bounds[1]))
        tray = ($Name -eq '06-kept')
    })
    Write-Host "Captured $Name."
}

try {
    Invoke-Native $Python @('-c', "from PIL import Image; print('Pillow is ready.')")
    $meta = Invoke-Gh @('api', "repos/$fixture") | ConvertFrom-Json
    Assert-Demo ($meta.full_name -ceq $fixture -and -not $meta.private -and $meta.permissions.push) 'the fixed public fixture is writable'
    if (-not $SkipBuild) { & "$PSScriptRoot\build.ps1" }
    $root = New-LiveRoot 'demo'
    New-Item -ItemType Directory -Force $shots | Out-Null
    $skills = Join-Path $root 'skills'
    $homeDir = Join-Path $root 'home'
    $report = Join-Path $root 'out\marks.txt'
    New-RunBranch $fixture 'main' $branch
    $createdBranch = $true
    $review = "---`nname: review-notes`ndescription: A checklist for a friendly code review.`n---`n# Review notes`n`nExplain why each change helps.`n"
    $tests = "---`nname: write-tests`ndescription: Start with a small, useful test.`n---`n# Write tests`n`nTest the behavior, not the implementation.`n"
    $checklist = "# Review checklist`n`nCheck the happy path.`n"
    $commit = Push-Upstream $fixture $branch @(
        @{ Path = 'demo-skills/review-notes/SKILL.md'; Text = $review },
        @{ Path = 'demo-skills/review-notes/references/checklist.md'; Text = $checklist },
        @{ Path = 'demo-skills/write-tests/SKILL.md'; Text = $tests }
    ) 'demo: two small skills'
    $up = Get-UpstreamFiles $fixture $commit 'demo-skills'
    Wait-PublicDemo $commit 'review-notes/SKILL.md' $up['review-notes/SKILL.md'].Id
    [IO.File]::WriteAllText((Join-Path $homeDir 'config.json'), '{"version":1,"default_interval":"5m","repos":[]}', $utf8)
    $envMap = @{ TSS_BENCH_REPORT = $report; TSS_API_BASE = $null; TSS_RAW_BASE = $null; TSS_THEME = $null }
    $child = Get-LiveChildEnv $root $envMap
    Assert-Demo ($child.TSS_HOME -eq $homeDir -and $child.TSS_SKILLS_DIR -eq $skills -and $child.TSS_NO_SAVED_TOKEN -eq '1') 'the child uses only throwaway app data and skips the saved token'
    $tray = Start-LiveTray $root @() $envMap
    $win = Open-Window $tray
    [void][TssLive]::SetWindowPos($win, [IntPtr]::Zero, 0, 0, 940, 620, 0x16)
    Click-Ui $win $UiPanel.Nav $UiId.NavAdd
    $ask = Wait-Live { $h = Find-Ask $tray; if ($h -ne [IntPtr]::Zero) { $h } } 'Add repository'
    Set-UiText (Get-Ui $ask $AskPanel $AskId.Edit) "https://github.com/$fixture/tree/$branch/demo-skills"
    Save-DemoScene $ask '01-add' 'Add a GitHub skills folder' 'One setup. No more copying each new version by hand.' `
        (Get-Ui $ask $AskPanel $AskId.Ok) 'Public fixture data only. No sign-in needed by the app.'
    Click-Ui $ask $AskPanel $AskId.Ok
    $offer = Wait-Live {
        $h = Find-Ask $tray
        if ($h -ne [IntPtr]::Zero -and (Read-Ui $h $AskPanel $AskId.Heading) -eq 'Start with Windows?') { $h }
    } 'the optional startup offer'
    Click-Ui $offer $AskPanel $AskId.Cancel
    [void](Wait-Live { (Find-Ask $tray) -eq [IntPtr]::Zero } 'Not now to close the offer')
    [void](Wait-Live { (Read-Hero $win).Title -eq 'All skills are up to date' } 'the first install' 120)
    [void](Open-Repo $win)
    Assert-Demo ((Compare-WithUpstream $skills $fixture $branch 'demo-skills').Count -eq 0) 'every installed file equals the fixture'
    Save-DemoScene $win '02-installed' 'Your skills are ready for Copilot' 'Installed on disk, in an isolated demo skills folder.' `
        (Get-Ui $win $UiPanel.Page $UiId.Skill) 'On disk: review-notes\SKILL.md and write-tests\SKILL.md'

    $updated = "# Review checklist`n`nCheck the happy path.`nCheck error messages too.`n"
    $polls = Get-MarkCount $report 'job-poll'
    $checks = Get-MarkCount $report 'job-check'
    $commit = Push-Upstream $fixture $branch @(@{ Path = 'demo-skills/review-notes/references/checklist.md'; Text = $updated }) 'demo: improve the checklist'
    $up = Get-UpstreamFiles $fixture $commit 'demo-skills'
    Wait-PublicDemo $commit 'review-notes/references/checklist.md' $up['review-notes/references/checklist.md'].Id
    Write-Host 'Waiting for the real five-minute scheduled check; no Sync now command is sent.'
    $localChecklist = Join-Path $skills 'review-notes\references\checklist.md'
    [void](Wait-Live {
        (Get-MarkCount $report 'job-poll') -gt $polls -and [IO.File]::ReadAllText($localChecklist) -eq $updated
    } 'the automatic update to reach disk' 420 1000)
    Assert-Demo ((Get-MarkCount $report 'job-check') -eq $checks) 'the update arrived without a manual check'
    Assert-Demo ((Compare-WithUpstream $skills $fixture $branch 'demo-skills').Count -eq 0) 'the scheduled update matches GitHub on disk'
    Save-DemoScene $win '03-updated' 'Updates arrive on their own' 'A real scheduled check installed the GitHub change.' `
        (Get-Ui $win $UiPanel.Page $UiId.Skill) '5-minute wait shortened. The usual default is once a day.'

    $localReview = Join-Path $skills 'review-notes\SKILL.md'
    $mine = $review + "`nMy rule: keep feedback kind and specific.`n"
    [IO.File]::WriteAllText($localReview, $mine, $utf8)
    [void](Wait-Live { @(Read-Skills $win | Where-Object Tag -eq 'Edited on this PC').Count -eq 1 } 'the local edit to appear')
    $commit = Push-Upstream $fixture $branch @(@{ Path = 'demo-skills/review-notes/SKILL.md'; Text = $review + "`nNew upstream rule: include one example.`n" }) 'demo: update an edited skill'
    $up = Get-UpstreamFiles $fixture $commit 'demo-skills'
    Wait-PublicDemo $commit 'review-notes/SKILL.md' $up['review-notes/SKILL.md'].Id
    [void](Invoke-TrayCheck $tray $report)
    Assert-Demo ([IO.File]::ReadAllText($localReview) -eq $mine) 'the upstream update did not overwrite the edit'
    $edited = @(Read-Skills $win | Where-Object Name -eq 'review-notes')[0]
    Assert-Demo ($edited.Detail -match 'Update held') 'the window explicitly reports the held update'
    Save-DemoScene $win '04-held' 'Your edits stop an update' 'This skill waits for you. Your words are still on disk.' `
        (Get-Ui $win $UiPanel.Page ($edited.Id + 1)) 'Sync now used here to show the conflict without another wait.'
    Click-Ui $win $UiPanel.Page ($edited.Id + 2)
    [void](Wait-Live { Read-Ui $win $UiPanel.Files $UiId.File } 'Show changes')
    Assert-Demo ((Read-Ui $win $UiPanel.Files $UiId.File) -match '^SKILL.md') 'Show changes compares the edited file'
    Save-DemoScene $win '05-diff' 'See the difference before choosing' 'Red is your line; green is the GitHub alternative.' `
        ([TssLive]::GetDlgItem($win, $UiPanel.Code)) 'Keep my version stops managing this skill; the others still sync.' 5000
    Assert-Demo ((Read-Ui $win $UiPanel.Head ($UiId.DiffAction + 1)) -eq 'Keep my version') 'the selected action keeps the local version'
    Click-Ui $win $UiPanel.Head ($UiId.DiffAction + 1)
    [void](Wait-Live { (Read-Hero $win).Title -eq 'All skills are up to date' } 'the resolution to finish')
    if ([TssLive]::Shown([TssLive]::GetDlgItem($win, $UiPanel.Head))) { Click-Ui $win $UiPanel.Head $UiId.Back }
    [void](Wait-Live { @(Read-Skills $win | Where-Object Tag -eq 'Kept as yours').Count -eq 1 } 'the kept skill')
    Assert-Demo ([IO.File]::ReadAllText($localReview) -eq $mine) 'Keep my version preserved the edit'
    Assert-Demo ([IO.File]::ReadAllText((Join-Path $skills 'write-tests\SKILL.md')) -eq $tests) 'the other skill is unchanged'
    Save-DemoScene $win '06-kept' 'Your version stays yours' 'Everything is settled. The other skills stay in sync.' `
        (Get-Ui $win $UiPanel.Hero $UiId.HeroTitle) 'Blue tray icon: all is well (enlarged illustration).'
    Close-Window $tray
    $manifest = Join-Path $shots 'story.json'
    [IO.File]::WriteAllText($manifest, (ConvertTo-Json -InputObject $scenes.ToArray() -Depth 5), $utf8)
    $first = Join-Path $shots 'demo.gif'
    $second = Join-Path $shots 'repeat.gif'
    Invoke-Native $Python @("$PSScriptRoot\demo_gif.py", $manifest, $first)
    Invoke-Native $Python @("$PSScriptRoot\demo_gif.py", $manifest, $second)
    Assert-Demo ((Get-FileHash $first).Hash -eq (Get-FileHash $second).Hash) 'the same frames encode to identical GIF bytes'
    Copy-Item -LiteralPath $first -Destination (Join-Path $RepoRoot 'docs\demo.gif')
    Write-Host ('Made docs\demo.gif; review frames in ' + $shots.Substring($RepoRoot.Length + 1))
} catch {
    Write-Host ("Demo failed at line $($_.InvocationInfo.ScriptLineNumber): " + ($_.Exception.Message -replace '[^\x00-\x7F]', '?')) -ForegroundColor Red
    $failed = $true
} finally {
    try {
        Stop-LiveTray $tray
        if ($createdBranch) {
            try { Remove-RunBranch $fixture $branch; Write-Host "Deleted fixture branch $branch." }
            catch {
                Write-Host "CLEANUP FAILED: delete $fixture branch $branch before finishing." -ForegroundColor Red
                $failed = $true
            }
        }
        if ($root) {
            Remove-LiveRoot $root
            if (Test-Path -LiteralPath $root) { Write-Host 'CLEANUP FAILED: the demo folder remains.' -ForegroundColor Red; $failed = $true }
            else { Write-Host 'Deleted the throwaway demo install.' }
        }
    } finally {
        $env:GH_HOST = $savedGhHost
    }
}
if ($failed) { exit 1 }
