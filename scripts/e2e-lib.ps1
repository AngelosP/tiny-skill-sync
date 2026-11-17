# Shared by the live scripts (e2e.ps1, soak.ps1): a throwaway install of the app, driving its windows, and a GitHub
# fixture repository to push changes to. Dot-source it after common.ps1: . "$PSScriptRoot\e2e-lib.ps1"
# Works in Windows PowerShell 5.1 and PowerShell 7. Needs the network and a signed-in GitHub CLI (gh) that may push.
#
# Nothing here touches the real skills folder, the real settings or the saved GitHub token: every program is started
# with TSS_HOME and TSS_SKILLS_DIR pointing into a throwaway folder in %TEMP%.

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Text;

public static class TssLive {
    delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc proc, IntPtr lparam);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassNameW(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("kernel32.dll")] public static extern bool QueryProcessCycleTime(IntPtr process, out ulong cycles);

    [StructLayout(LayoutKind.Sequential)] struct RECT { public int Left, Top, Right, Bottom; }

    // The first top-level window of this class that belongs to the process (visible or not), or zero.
    public static IntPtr Find(uint pid, string cls, bool visibleOnly) {
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate (IntPtr h, IntPtr l) {
            uint owner;
            GetWindowThreadProcessId(h, out owner);
            if (owner != pid) return true;
            StringBuilder sb = new StringBuilder(256);
            GetClassNameW(h, sb, 256);
            if (sb.ToString() != cls) return true;
            if (visibleOnly && !IsWindowVisible(h)) return true;
            found = h;
            return false;
        }, IntPtr.Zero);
        return found;
    }

    // Saves what the window looks like as a PNG, whether or not something covers it. Returns "WIDTHxHEIGHT".
    // Not to be trusted for what the window has drawn: of captures of a window that was drawn completely and had nothing
    // left to paint, about two in three came back with the lower controls missing (CaptureScreen, of the same window
    // at the same moments, was complete every time). Use it only for a window that something covers.
    public static string Capture(IntPtr hwnd, string path) {
        RECT r;
        if (!GetWindowRect(hwnd, out r)) throw new Exception("GetWindowRect failed");
        int w = r.Right - r.Left, h = r.Bottom - r.Top;
        if (w <= 0 || h <= 0) throw new Exception("the window has no size");
        using (Bitmap bmp = new Bitmap(w, h))
        using (Graphics g = Graphics.FromImage(bmp)) {
            IntPtr hdc = g.GetHdc();
            try { PrintWindow(hwnd, hdc, 2); } finally { g.ReleaseHdc(hdc); }
            bmp.Save(path, ImageFormat.Png);
        }
        return w + "x" + h;
    }

    // The same, from the screen itself: what a person sees where the window is (so something on top of it is in the
    // picture too). Pop-up menus can only be captured this way.
    public static string CaptureScreen(IntPtr hwnd, string path, int pad) {
        RECT r;
        if (!GetWindowRect(hwnd, out r)) throw new Exception("GetWindowRect failed");
        int w = r.Right - r.Left + 2 * pad, h = r.Bottom - r.Top + 2 * pad;
        using (Bitmap bmp = new Bitmap(w, h))
        using (Graphics g = Graphics.FromImage(bmp)) {
            g.CopyFromScreen(r.Left - pad, r.Top - pad, 0, 0, new Size(w, h));
            bmp.Save(path, ImageFormat.Png);
        }
        return w + "x" + h;
    }

    public static string Bounds(IntPtr hwnd) {
        RECT r;
        GetWindowRect(hwnd, out r);
        return r.Left + "," + r.Top + "," + r.Right + "," + r.Bottom;
    }

    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT point);

    // Where the mouse pointer is on the screen: "x,y".
    public static string CursorAt() {
        POINT p;
        GetCursorPos(out p);
        return p.X + "," + p.Y;
    }

    [DllImport("user32.dll")] static extern bool GetUpdateRect(IntPtr hwnd, out RECT rect, bool erase);
    [DllImport("user32.dll")] static extern IntPtr WindowFromPoint(POINT point);
    [DllImport("user32.dll")] static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);

    // What the window still has to paint: "window" if the window itself has, and the id of each shown panel or control
    // inside it, at any depth, that has.
    public static string Pending(IntPtr parent) {
        StringBuilder sb = new StringBuilder();
        RECT r;
        if (GetUpdateRect(parent, out r, false)) sb.Append("window ");
        PendingIn(parent, sb);
        return sb.ToString().Trim();
    }

    static void PendingIn(IntPtr parent, StringBuilder sb) {
        RECT r;
        for (IntPtr h = GetWindow(parent, 5); h != IntPtr.Zero; h = GetWindow(h, 2)) {
            if (!IsWindowVisible(h)) continue;
            if (GetUpdateRect(h, out r, false)) sb.Append(GetDlgCtrlID(h) + " ");
            PendingIn(h, sb);
        }
    }

    public static bool Shown(IntPtr hwnd) { return IsWindowVisible(hwnd); }

    // Waits until the desktop has been composed again, so that what was just painted is on the screen.
    [DllImport("dwmapi.dll")] public static extern int DwmFlush();

    // How much a part of a picture varies: the largest difference in any one colour between two of its pixels. A part
    // that was drawn has text, a glyph or a border in it; one that was not is a single flat colour.
    public static int Contrast(string path, int x, int y, int w, int h) {
        using (Bitmap bmp = new Bitmap(path)) {
            int[] lo = new int[] { 255, 255, 255 };
            int[] hi = new int[] { 0, 0, 0 };
            for (int j = Math.Max(0, y); j < Math.Min(bmp.Height, y + h); j++) {
                for (int i = Math.Max(0, x); i < Math.Min(bmp.Width, x + w); i++) {
                    Color c = bmp.GetPixel(i, j);
                    int[] v = new int[] { c.R, c.G, c.B };
                    for (int k = 0; k < 3; k++) { lo[k] = Math.Min(lo[k], v[k]); hi[k] = Math.Max(hi[k], v[k]); }
                }
            }
            return Math.Max(0, Math.Max(hi[0] - lo[0], Math.Max(hi[1] - lo[1], hi[2] - lo[2])));
        }
    }

    // Whether another window is on top of this one: the process and class of what is on top at the corners and the
    // middle of the window, or "" when the window itself is on top at all of them.
    public static string CoveredBy(IntPtr hwnd) {
        RECT r;
        GetWindowRect(hwnd, out r);
        int[] xs = new int[] { r.Left + 24, r.Right - 24, r.Left + 24, r.Right - 24, (r.Left + r.Right) / 2 };
        int[] ys = new int[] { r.Top + 24, r.Top + 24, r.Bottom - 24, r.Bottom - 24, (r.Top + r.Bottom) / 2 };
        for (int i = 0; i < xs.Length; i++) {
            POINT p = new POINT();
            p.X = xs[i];
            p.Y = ys[i];
            IntPtr at = WindowFromPoint(p);
            IntPtr top = at == IntPtr.Zero ? IntPtr.Zero : GetAncestor(at, 2);
            if (top == hwnd) continue;
            if (top == IntPtr.Zero) return "nothing";
            uint pid;
            GetWindowThreadProcessId(top, out pid);
            StringBuilder cls = new StringBuilder(64);
            GetClassNameW(top, cls, 64);
            return pid + " " + cls;
        }
        return "";
    }

    [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr hwnd, uint cmd);
    [DllImport("user32.dll")] static extern int GetWindowLongW(IntPtr hwnd, int index);
    [DllImport("user32.dll")] static extern int GetDlgCtrlID(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }

    // The direct children of a window in z-order, which is the order Tab visits them in, each as
    // "id|class|tab or -|shown or hidden|on or off|x,y,width,height|style in hex|handle" (x and y inside the parent's
    // client area).
    public static string[] Children(IntPtr parent) {
        System.Collections.Generic.List<string> list = new System.Collections.Generic.List<string>();
        POINT origin = new POINT();
        ClientToScreen(parent, ref origin);
        for (IntPtr h = GetWindow(parent, 5); h != IntPtr.Zero; h = GetWindow(h, 2)) {
            StringBuilder cls = new StringBuilder(64);
            GetClassNameW(h, cls, 64);
            RECT r;
            GetWindowRect(h, out r);
            int style = GetWindowLongW(h, -16);
            list.Add(string.Format("{0}|{1}|{2}|{3}|{4}|{5},{6},{7},{8}|{9:X8}|{10}", GetDlgCtrlID(h), cls, (style & 0x10000) != 0 ? "tab" : "-",
                IsWindowVisible(h) ? "shown" : "hidden", IsWindowEnabled(h) ? "on" : "off", r.Left - origin.X, r.Top - origin.Y, r.Right - r.Left, r.Bottom - r.Top,
                style, h.ToInt64()));
        }
        return list.ToArray();
    }

    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern IntPtr SendMessageTimeoutW(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp, uint flags, uint timeout, out IntPtr result);
    [DllImport("user32.dll")] static extern int GetMenuItemCount(IntPtr menu);
    [DllImport("user32.dll")] static extern uint GetMenuState(IntPtr menu, uint pos, uint flags);
    [DllImport("user32.dll")] static extern uint GetMenuDefaultItem(IntPtr menu, uint byPos, uint flags);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetMenuStringW(IntPtr menu, uint pos, StringBuilder text, int max, uint flags);
    [DllImport("shell32.dll")] static extern int Shell_NotifyIconGetRect(ref NOTIFYICONIDENTIFIER id, out RECT rect);
    [StructLayout(LayoutKind.Sequential)] struct NOTIFYICONIDENTIFIER { public uint cbSize; public IntPtr hWnd; public uint uID; public Guid guidItem; }

    // The size of the window's client area: "width,height".
    public static string Client(IntPtr hwnd) {
        RECT r;
        GetClientRect(hwnd, out r);
        return (r.Right - r.Left) + "," + (r.Bottom - r.Top);
    }

    // How long the window takes to answer a message, in milliseconds; -1 when it does not answer within the time.
    public static long Ping(IntPtr hwnd, uint timeoutMs) {
        System.Diagnostics.Stopwatch sw = System.Diagnostics.Stopwatch.StartNew();
        IntPtr result;
        IntPtr ok = SendMessageTimeoutW(hwnd, 0, IntPtr.Zero, IntPtr.Zero, 2, timeoutMs, out result);
        return ok == IntPtr.Zero ? -1 : sw.ElapsedMilliseconds;
    }

    // The items of a pop-up menu that is showing (the window of class #32768), top to bottom: the text, "-" for a
    // separator, and " (default)" or " (grey)" after an item that is.
    public static string[] MenuItems(IntPtr menuWindow) {
        IntPtr menu = SendMessageW(menuWindow, 0x01E1, IntPtr.Zero, IntPtr.Zero);
        System.Collections.Generic.List<string> list = new System.Collections.Generic.List<string>();
        if (menu == IntPtr.Zero) return list.ToArray();
        int n = GetMenuItemCount(menu);
        uint def = GetMenuDefaultItem(menu, 1, 1);
        for (int i = 0; i < n; i++) {
            uint state = GetMenuState(menu, (uint)i, 0x400);
            if ((state & 0x800) != 0) { list.Add("-"); continue; }
            StringBuilder sb = new StringBuilder(128);
            GetMenuStringW(menu, (uint)i, sb, 128, 0x400);
            string text = sb.ToString();
            if (def == (uint)i) text += " (default)";
            if ((state & 3) != 0) text += " (grey)";
            list.Add(text);
        }
        return list.ToArray();
    }

    // Where the app's icon is in the notification area, as [left, top, right, bottom]; null when the shell does not say.
    // An icon the shell keeps in its overflow is reported as the button that opens the overflow.
    public static int[] TrayIconRect(IntPtr hwnd, uint id) {
        NOTIFYICONIDENTIFIER n = new NOTIFYICONIDENTIFIER();
        n.cbSize = (uint)Marshal.SizeOf(typeof(NOTIFYICONIDENTIFIER));
        n.hWnd = hwnd;
        n.uID = id;
        RECT r;
        if (Shell_NotifyIconGetRect(ref n, out r) != 0) return null;
        return new int[] { r.Left, r.Top, r.Right, r.Bottom };
    }

    [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr parent, int id);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr SendMessageW(IntPtr hwnd, uint msg, IntPtr wp, StringBuilder lp);

    // The text of a window or control of any kind.
    public static string Text(IntPtr hwnd) {
        int n = (int)SendMessageW(hwnd, 0x000E, IntPtr.Zero, IntPtr.Zero);
        StringBuilder sb = new StringBuilder(n + 2);
        SendMessageW(hwnd, 0x000D, (IntPtr)(n + 1), sb);
        return sb.ToString();
    }
}
'@

[void][TssLive]::SetProcessDPIAware()

$WM_COMMAND = 0x111
$WM_CLOSE = 0x10
$WM_SETTEXT = 0xC
$WM_KEYDOWN = 0x100
$WM_CANCELMODE = 0x1F
$WM_EXITSIZEMOVE = 0x232
$BM_CLICK = 0xF5
$BM_GETCHECK = 0xF0
$MN_SELECTITEM = 0x1E5
$CBN_SELENDOK = 9
$VK_ESCAPE = 0x1B
$VK_RETURN = 0x0D
$CMD_SETTINGS = 1001
$CMD_CHECK = 1002
$CMD_QUIT = 1003
$TRAY_CLASS = 'TinySkillSyncTray'
$WINDOW_CLASS = 'TinySkillSyncWindow'
$ASK_CLASS = 'TinySkillSyncAsk'
$MENU_CLASS = '#32768'
$TRAY_ICON_ID = 1
$TRAY_CALLBACK = 0x8001
$WM_CONTEXTMENU = 0x7B

# The panels of the window, each a child of it, and the ids of the controls on them (src\win\ui\page.rs, mod id).
# Head, Files and Code show only while the window shows what differs in a skill; the others are hidden then.
$UiPanel = @{ Hero = 101; Nav = 102; Page = 103; Head = 104; Files = 105; Code = 106 }
# Save token and Delete token are left out on purpose: the saved token is shared by every copy of the app.
$UiId = @{
    Sync = 200; HeroTitle = 201; HeroSub = 202
    NavAdd = 300; NavActivity = 301; NavSettings = 302; NavHead = 303; NavRepo = 1000
    WelcomeAdd = 305; More = 310; Every = 311; Enabled = 312; Retry = 313; OpenSettings = 314
    DefaultEvery = 320; OpenSkills = 321; OpenBackups = 322
    Title = 400; Private = 401; Sub = 402; InfoLead = 403; InfoText = 404; SkillsDir = 410; TokenText = 411; Empty = 412
    Back = 330; DiffAction = 333; Note = 100000; File = 200000; Gap = 300000
    # Plus 8 per skill, plus the part: 0 the row ("name`ntag"), 1 its details, 2 Show changes, 3 Open folder, 4 and on
    # its actions.
    Skill = 1000000
}
# A question (src\win\ui\page.rs, mod ask_id): its controls are on one panel.
$AskPanel = 100
$AskId = @{ Ok = 1; Cancel = 2; Edit = 10; Heading = 11; Text = 12; Label = 13; Hint = 14; ComboLabel = 15; Combo = 16; Note = 17; Choice = 20 }

# ---------------------------------------------------------------- a throwaway install

# A folder in %TEMP% with its own settings, skills and copies of the release executables (a rebuild does not
# disturb a run that is still going).
function New-LiveRoot([string]$Prefix) {
    $root = Join-Path $env:TEMP ("tss-$Prefix-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    foreach ($d in 'home', 'skills', 'bin', 'out') { New-Item -ItemType Directory -Force (Join-Path $root $d) | Out-Null }
    $from = Get-ExeDir 'release'
    foreach ($exe in 'tiny-skill-sync.exe', 'tss.exe') {
        if (-not (Test-Path (Join-Path $from $exe))) { throw "$exe is not built. Run scripts\build.ps1 first." }
        Copy-Item (Join-Path $from $exe) (Join-Path $root 'bin')
    }
    $root
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
    # A $null value removes the variable.
    foreach ($key in @($Env.Keys)) {
        if ($null -eq $Env[$key]) { $psi.EnvironmentVariables.Remove($key) } else { $psi.EnvironmentVariables[$key] = [string]$Env[$key] }
    }
    $psi
}

# The environment a program started with these settings gets, as cmd.exe reports it: name -> value. Lets a scenario
# that removes TSS_HOME and TSS_SKILLS_DIR prove which folders the app would use before it starts the app.
function Get-LiveChildEnv($Root, [hashtable]$Env = @{}) {
    $psi = New-LiveStartInfo $Root 'tss.exe' @() $Env
    $psi.FileName = $env:ComSpec
    $psi.Arguments = '/d /c set'
    $psi.RedirectStandardOutput = $true
    $p = [Diagnostics.Process]::Start($psi)
    $text = $p.StandardOutput.ReadToEnd()
    $p.WaitForExit()
    $map = @{}
    foreach ($line in ($text -split "`r?`n")) {
        $kv = $line -split '=', 2
        if ($kv.Count -eq 2 -and $kv[0]) { $map[$kv[0]] = $kv[1] }
    }
    $map
}

# Starts the tray app on the throwaway install.
function Start-LiveTray($Root, [string[]]$AppArgs = @(), [hashtable]$Env = @{}) {
    $psi = New-LiveStartInfo $Root 'tiny-skill-sync.exe' $AppArgs $Env
    [Diagnostics.Process]::Start($psi)
}

# Runs the command line on the throwaway install and returns its exit code and everything it printed.
function Invoke-LiveTss($Root, [string[]]$TssArgs, [hashtable]$Env = @{}, [int]$TimeoutSec = 240) {
    Assert-NotTokenChange $TssArgs
    $psi = New-LiveStartInfo $Root 'tss.exe' $TssArgs $Env
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [Text.Encoding]::UTF8
    $p = [Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit($TimeoutSec * 1000)) {
        Stop-Process -Id $p.Id -Force
        throw "tss $($TssArgs -join ' ') did not finish within $TimeoutSec s and was stopped."
    }
    $p.WaitForExit()
    [pscustomobject]@{ Code = $p.ExitCode; Text = ($out.Result + $err.Result).TrimEnd() }
}

function Find-LiveWindow([int]$ProcId, [string]$Class, [switch]$Visible) {
    [TssLive]::Find([uint32]$ProcId, $Class, [bool]$Visible)
}

# Asks the tray app to do something, the way its own menu does.
function Send-LiveCommand([Diagnostics.Process]$P, [int]$Command) {
    $h = Find-LiveWindow $P.Id $TRAY_CLASS
    if ($h -eq [IntPtr]::Zero) { throw "The app (pid $($P.Id)) has no main window." }
    [void][TssLive]::PostMessageW($h, $WM_COMMAND, [IntPtr]$Command, [IntPtr]::Zero)
}

function Stop-LiveTray([Diagnostics.Process]$P) {
    if ($null -eq $P -or $P.HasExited) { return }
    $h = Find-LiveWindow $P.Id $TRAY_CLASS
    if ($h -ne [IntPtr]::Zero) { [void][TssLive]::PostMessageW($h, $WM_COMMAND, [IntPtr]$CMD_QUIT, [IntPtr]::Zero) }
    if (-not $P.WaitForExit(8000)) {
        Stop-Process -Id $P.Id -Force
        Write-Host "The app did not quit within 8 s and was stopped." -ForegroundColor Yellow
    }
}

function Remove-LiveRoot([string]$Root) {
    if ($Root -and (Test-Path $Root) -and (Split-Path $Root -Leaf) -match '^tss-[a-z0-9]+-[0-9a-f]{8}$') {
        Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue
    }
}

# Waits until the script block returns something true; returns it. Throws after the time is up.
function Wait-Live([scriptblock]$Until, [string]$What, [int]$TimeoutSec = 60, [int]$EveryMs = 300) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    do {
        $r = & $Until
        if ($r) { return $r }
        Start-Sleep -Milliseconds $EveryMs
    } while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec)
    throw "Gave up after $TimeoutSec s waiting for: $What"
}

# ---------------------------------------------------------------- what is on disk

$script:Sha1 = [Security.Cryptography.SHA1]::Create()

# The id Git gives a file's content, so a file on disk can be compared with a GitHub listing.
function Get-BlobId([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    $head = [Text.Encoding]::ASCII.GetBytes("blob $($bytes.Length)`0")
    $all = New-Object byte[] ($head.Length + $bytes.Length)
    [Array]::Copy($head, $all, $head.Length)
    [Array]::Copy($bytes, 0, $all, $head.Length, $bytes.Length)
    ([BitConverter]::ToString($script:Sha1.ComputeHash($all)) -replace '-', '').ToLowerInvariant()
}

# Every file below a folder: relative path (with /) -> blob id. A link or junction is reported as "(link)".
function Get-DiskFiles([string]$Dir) {
    $map = [ordered]@{}
    if (-not (Test-Path -LiteralPath $Dir)) { return $map }
    $base = (Resolve-Path -LiteralPath $Dir).Path.TrimEnd('\')
    Get-ChildItem -LiteralPath $Dir -Recurse -File -Force -ErrorAction SilentlyContinue | Sort-Object FullName | ForEach-Object {
        $rel = $_.FullName.Substring($base.Length + 1).Replace('\', '/')
        $map[$rel] = if ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) { '(link)' } else { Get-BlobId $_.FullName }
    }
    $map
}

# One line that changes when anything under the folder changes: names, content and last-write times.
function Get-DiskStamp([string]$Dir) {
    if (-not (Test-Path -LiteralPath $Dir)) { return 'absent' }
    $text = Get-ChildItem -LiteralPath $Dir -Recurse -File -Force -ErrorAction SilentlyContinue | Sort-Object FullName |
        ForEach-Object { '{0}|{1}|{2}' -f $_.FullName, $_.Length, $_.LastWriteTimeUtc.Ticks }
    $bytes = [Text.Encoding]::UTF8.GetBytes(($text -join "`n"))
    ([BitConverter]::ToString([Security.Cryptography.SHA1]::Create().ComputeHash($bytes)) -replace '-', '')
}

# ---------------------------------------------------------------- GitHub fixtures (through gh)

function Invoke-Gh([string[]]$GhArgs) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $out = & gh @GhArgs 2>&1 } finally { $ErrorActionPreference = $saved }
    if ($LASTEXITCODE -ne 0) { throw "gh $($GhArgs -join ' ') failed: $($out -join ' ')" }
    ($out | ForEach-Object { "$_" }) -join "`n"
}

# Only branches made for a run may be deleted, never a fixture's own.
function Assert-RunBranch([string]$Branch) {
    if ($Branch -notmatch '^(e2e|soak)-[0-9a-z-]+$') { throw "'$Branch' is not a branch this script made; refusing to change it." }
}

function New-RunBranch([string]$Repo, [string]$From, [string]$Branch) {
    Assert-RunBranch $Branch
    $sha = Invoke-Gh @('api', "repos/$Repo/git/ref/heads/$From", '--jq', '.object.sha')
    [void](Invoke-Gh @('api', '-X', 'POST', "repos/$Repo/git/refs", '-f', "ref=refs/heads/$Branch", '-f', "sha=$sha"))
}

function Remove-RunBranch([string]$Repo, [string]$Branch) {
    Assert-RunBranch $Branch
    [void](Invoke-Gh @('api', '-X', 'DELETE', "repos/$Repo/git/refs/heads/$Branch"))
}

# The files (blobs) of a folder on a branch: relative path -> @{ Id; Mode }.
function Get-UpstreamFiles([string]$Repo, [string]$Ref, [string]$Path = '') {
    $spec = if ($Path) { "${Ref}:${Path}" } else { $Ref }
    $json = Invoke-Gh @('api', "repos/$Repo/git/trees/${spec}?recursive=1")
    $tree = $json | ConvertFrom-Json
    if ($tree.truncated) { throw 'the listing is truncated' }
    $map = [ordered]@{}
    foreach ($e in $tree.tree) { if ($e.type -eq 'blob') { $map[[string]$e.path] = @{ Id = $e.sha; Mode = $e.mode } } }
    $map
}

# Which files a check installs from a listing of the folder: every immediate child folder that has a SKILL.md at
# its root is a skill (the plain rule; the hostile scenario spells out its own expectations).
function Get-ExpectedFiles($Upstream) {
    $skills = @{}
    foreach ($path in $Upstream.Keys) {
        $parts = $path -split '/', 2
        if ($parts.Count -eq 2 -and $parts[1] -eq 'SKILL.md' -and -not $parts[0].StartsWith('.')) { $skills[$parts[0]] = $true }
    }
    $map = [ordered]@{}
    foreach ($path in $Upstream.Keys) {
        $top = ($path -split '/', 2)[0]
        if ($skills.ContainsKey($top) -and $path.Contains('/')) { $map[$path] = $Upstream[$path].Id }
    }
    $map
}

# What differs between the skills folder and what upstream says should be installed: a list of sentences, empty
# when they agree.
function Compare-WithUpstream([string]$SkillsDir, [string]$Repo, [string]$Ref, [string]$Path) {
    $want = Get-ExpectedFiles (Get-UpstreamFiles $Repo $Ref $Path)
    $have = Get-DiskFiles $SkillsDir
    $diff = New-Object System.Collections.Generic.List[string]
    foreach ($k in $want.Keys) {
        if (-not $have.Contains($k)) { $diff.Add("missing: $k") }
        elseif ($have[$k] -ne $want[$k]) { $diff.Add("different: $k") }
    }
    foreach ($k in $have.Keys) { if (-not $want.Contains($k)) { $diff.Add("unexpected: $k") } }
    , @($diff)
}

# Pushes one commit that adds, changes and deletes files. A change is @{ Path; Text } for a file that is written,
# @{ Path; Delete = $true } for one that goes, @{ DeleteFolder = 'skills/x' } for every file below a folder.
# GitHub can answer the read of the branch with the head from a moment ago (see the plan, "GitHub's cache"); a commit
# built on that cannot move the branch, so the whole push is done again from a new read.
function Push-Upstream([string]$Repo, [string]$Branch, [object[]]$Changes, [string]$Message = 'live test change') {
    Assert-RunBranch $Branch
    for ($try = 1; ; $try++) {
        try { return (Push-UpstreamOnce $Repo $Branch $Changes $Message) }
        catch {
            if ($try -ge 6 -or $_.Exception.Message -notmatch 'not a fast forward') { throw }
            Start-Sleep -Seconds 3
        }
    }
}

function Push-UpstreamOnce([string]$Repo, [string]$Branch, [object[]]$Changes, [string]$Message) {
    $head = Invoke-Gh @('api', "repos/$Repo/git/ref/heads/$Branch", '--jq', '.object.sha')
    $baseTree = Invoke-Gh @('api', "repos/$Repo/git/commits/$head", '--jq', '.tree.sha')
    $entries = New-Object System.Collections.Generic.List[object]
    foreach ($c in $Changes) {
        if ($c.ContainsKey('DeleteFolder')) {
            foreach ($path in (Get-UpstreamFiles $Repo $Branch).Keys) {
                if ($path.StartsWith($c.DeleteFolder + '/')) { $entries.Add(@{ path = $path; mode = '100644'; type = 'blob'; sha = $null }) }
            }
        } elseif ($c.ContainsKey('Delete')) {
            $entries.Add(@{ path = $c.Path; mode = '100644'; type = 'blob'; sha = $null })
        } else {
            $entries.Add(@{ path = $c.Path; mode = '100644'; type = 'blob'; content = $c.Text })
        }
    }
    $body = Join-Path $env:TEMP ('tss-gh-body-' + [guid]::NewGuid().ToString('N').Substring(0, 8) + '.json')
    try {
        $utf8 = New-Object Text.UTF8Encoding($false)
        [IO.File]::WriteAllText($body, (@{ base_tree = $baseTree; tree = $entries.ToArray() } | ConvertTo-Json -Depth 6), $utf8)
        $tree = Invoke-Gh @('api', '-X', 'POST', "repos/$Repo/git/trees", '--input', $body, '--jq', '.sha')
        [IO.File]::WriteAllText($body, (@{ message = $Message; tree = $tree; parents = @($head) } | ConvertTo-Json -Depth 4), $utf8)
        $commit = Invoke-Gh @('api', '-X', 'POST', "repos/$Repo/git/commits", '--input', $body, '--jq', '.sha')
    } finally { Remove-Item -Force $body -ErrorAction SilentlyContinue }
    [void](Invoke-Gh @('api', '-X', 'PATCH', "repos/$Repo/git/refs/heads/$Branch", '-f', "sha=$commit"))
    $commit
}

# The changes a fixture branch (skills under skills/, as in the public fixture) goes through, one per round, and
# over again after six. Each says what to push and how to tell, on disk, that it arrived.
function Get-FixtureChange([int]$N, [string]$SkillsDir, [string]$Marker) {
    $skill = Join-Path $SkillsDir 'soak-new'
    switch ((($N - 1) % 6) + 1) {
        1 { @{ What = 'add a skill'
               Push = @(@{ Path = 'skills/soak-new/SKILL.md'; Text = "---`nname: soak-new`ndescription: made by a live test`n---`n$Marker`n" }, @{ Path = 'skills/soak-new/references/notes.md'; Text = "notes $Marker`n" })
               Arrived = { Test-Path -LiteralPath (Join-Path $skill 'SKILL.md') }.GetNewClosure() } }
        2 { @{ What = 'edit a file'
               Push = @(@{ Path = 'skills/alpha/references/guide.md'; Text = "# Guide`n`nchanged by a live test: $Marker`n" })
               Arrived = { $f = Join-Path $SkillsDir 'alpha\references\guide.md'; (Test-Path -LiteralPath $f) -and ((Get-Content -LiteralPath $f -Raw) -match [regex]::Escape($Marker)) }.GetNewClosure() } }
        3 { @{ What = 'add a file to a skill'
               Push = @(@{ Path = 'skills/soak-new/extra.md'; Text = "extra $Marker`n" })
               Arrived = { Test-Path -LiteralPath (Join-Path $skill 'extra.md') }.GetNewClosure() } }
        4 { @{ What = 'delete a skill'
               Push = @(@{ DeleteFolder = 'skills/gamma' })
               Arrived = { -not (Test-Path -LiteralPath (Join-Path $SkillsDir 'gamma')) }.GetNewClosure() } }
        5 { @{ What = 'edit a skill'
               Push = @(@{ Path = 'skills/soak-new/SKILL.md'; Text = "---`nname: soak-new`ndescription: edited by a live test`n---`n$Marker`n" })
               Arrived = { $f = Join-Path $skill 'SKILL.md'; (Test-Path -LiteralPath $f) -and ((Get-Content -LiteralPath $f -Raw) -match [regex]::Escape($Marker)) }.GetNewClosure() } }
        6 { @{ What = 'bring a skill back'
               Push = @(@{ Path = 'skills/gamma/SKILL.md'; Text = "---`nname: gamma`ndescription: back again`n---`n$Marker`n" })
               Arrived = { Test-Path -LiteralPath (Join-Path $SkillsDir 'gamma\SKILL.md') }.GetNewClosure() } }
    }
}

# ---------------------------------------------------------------- what a tray app reports (TSS_BENCH_REPORT)

# The marks a tray app wrote to its report file (see src\win\ui\bench.rs), oldest first. A mark per finished job:
# job-startup, job-look, job-poll, job-check, job-resolve, ...
function Read-Marks([string]$Path) {
    $lines = @()
    try {
        $fs = New-Object IO.FileStream($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
        try { $lines = @((New-Object IO.StreamReader($fs)).ReadToEnd() -split "`r`n" | Where-Object { $_ }) } finally { $fs.Dispose() }
    } catch { return @() }
    foreach ($line in $lines) {
        $m = @{}
        foreach ($pair in $line -split ' ') { $kv = $pair -split '=', 2; if ($kv.Count -eq 2) { $m[$kv[0]] = $kv[1] } }
        $took = $null
        if ($m.ContainsKey('took_ms')) { $took = [double]$m['took_ms'] }
        [pscustomobject]@{ Mark = $m['mark']; AtMs = [double]$m['at_ms']; CpuMs = [double]$m['cpu_ms']; TookMs = $took }
    }
}

function Get-MarkCount([string]$Path, [string]$Mark) {
    @(Read-Marks $Path | Where-Object { $_.Mark -eq $Mark }).Count
}

# The tray app's "Sync now": asks for it the way the menu does and waits until that job has finished. Returns how
# long the app says the job took, in milliseconds.
function Invoke-TrayCheck([Diagnostics.Process]$P, [string]$Report, [int]$TimeoutSec = 120) {
    $before = Get-MarkCount $Report 'job-check'
    Send-LiveCommand $P $CMD_CHECK
    [void](Wait-Live { (Get-MarkCount $Report 'job-check') -gt $before } 'the check to finish' $TimeoutSec 100)
    $last = @(Read-Marks $Report | Where-Object { $_.Mark -eq 'job-check' }) | Select-Object -Last 1
    $last.TookMs
}

# Waits until no job has finished for a few seconds in a row. Call it after waiting for the job you expect, since a
# job that is still running has not left its mark yet.
function Wait-Quiet([string]$Report, [int]$QuietSec = 4, [int]$TimeoutSec = 90) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $since = [Diagnostics.Stopwatch]::StartNew()
    $last = -1
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        $n = @(Read-Marks $Report).Count
        if ($n -ne $last) { $last = $n; $since.Restart() }
        elseif ($n -gt 0 -and $since.Elapsed.TotalSeconds -ge $QuietSec) { return $true }
        Start-Sleep -Milliseconds 250
    }
    $false
}

# ---------------------------------------------------------------- the window, read and driven from outside

# Opens the window the way the tray menu does (or finds it open), and returns it once it shows.
function Open-Window([Diagnostics.Process]$P) {
    $h = Find-LiveWindow $P.Id $WINDOW_CLASS -Visible
    if ($h -eq [IntPtr]::Zero) { Send-LiveCommand $P $CMD_SETTINGS }
    Wait-Live { $w = Find-LiveWindow $P.Id $WINDOW_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 'the window' 15 100
}

function Close-Window([Diagnostics.Process]$P) {
    $h = Find-LiveWindow $P.Id $WINDOW_CLASS
    if ($h -ne [IntPtr]::Zero) { [void][TssLive]::PostMessageW($h, $WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero) }
}

# A control on a panel of the window (or of a question), or zero when it is not there. The window destroys the controls
# it no longer shows, so one that is there is shown whenever its panel is. A long page makes its controls only within
# about two screens of what is shown (control.rs, NEAR), so a control further down is zero until the page scrolls to
# it; the fixtures' pages are short enough that every control is made.
function Find-Ui([IntPtr]$Window, [int]$Panel, [int]$Id) {
    $p = [TssLive]::GetDlgItem($Window, $Panel)
    if ($p -eq [IntPtr]::Zero) { return [IntPtr]::Zero }
    [TssLive]::GetDlgItem($p, $Id)
}

function Get-Ui([IntPtr]$Window, [int]$Panel, [int]$Id) {
    $h = Find-Ui $Window $Panel $Id
    if ($h -eq [IntPtr]::Zero) { throw "The window has no control $Id on panel $Panel." }
    $h
}

# The text of a control (every label and paragraph is a control too), or $null when it is not there.
function Read-Ui([IntPtr]$Window, [int]$Panel, [int]$Id) {
    $h = Find-Ui $Window $Panel $Id
    if ($h -eq [IntPtr]::Zero) { return $null }
    [TssLive]::Text($h)
}

# Clicks a button, a row, a toggle or a choice as a person would: the control changes its own look, then tells the
# window. Never a list: a click opens its menu, which waits for a person (use Choose-Ui).
function Click-Ui([IntPtr]$Window, [int]$Panel, [int]$Id) {
    [void][TssLive]::PostMessageW((Get-Ui $Window $Panel $Id), $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
}

# Whether a toggle or a choice is on.
function Get-Checked([IntPtr]$Window, [int]$Panel, [int]$Id) {
    ([TssLive]::SendMessageW((Get-Ui $Window $Panel $Id), $BM_GETCHECK, [IntPtr]::Zero, [IntPtr]::Zero).ToInt64()) -eq 1
}

# Gives a control a text through its own message. A text box takes it as typing, and tells its question.
function Set-UiText([IntPtr]$Control, [string]$Text) {
    $p = [Runtime.InteropServices.Marshal]::StringToHGlobalUni($Text)
    try { [void][TssLive]::SendMessageW($Control, $WM_SETTEXT, [IntPtr]::Zero, $p) } finally { [Runtime.InteropServices.Marshal]::FreeHGlobal($p) }
}

# Picks an entry of a list as its menu does: the list shows the entry, then tells the window, through its panel, that
# the choice is made.
function Choose-Ui([IntPtr]$Window, [int]$Panel, [int]$Id, [string]$Entry) {
    $list = Get-Ui $Window $Panel $Id
    Set-UiText $list $Entry
    [void][TssLive]::PostMessageW([TssLive]::GetDlgItem($Window, $Panel), $WM_COMMAND, [IntPtr](($CBN_SELENDOK -shl 16) -bor $Id), $list)
}

# The big line at the top of the window and the one under it.
function Read-Hero([IntPtr]$Window) {
    [pscustomobject]@{ Title = (Read-Ui $Window $UiPanel.Hero $UiId.HeroTitle); Sub = (Read-Ui $Window $UiPanel.Hero $UiId.HeroSub) }
}

# The repositories in the list at the left, top to bottom: the label and the line under it. Wrap the call in @( ).
function Read-Nav([IntPtr]$Window) {
    for ($i = 0; ; $i++) {
        $t = Read-Ui $Window $UiPanel.Nav ($UiId.NavRepo + $i)
        if ($null -eq $t) { break }
        $two = $t -split "`n", 2
        [pscustomobject]@{ Index = $i; Label = $two[0]; Sub = $(if ($two.Count -gt 1) { $two[1] } else { '' }) }
    }
}

# The skills on a repository's page, top to bottom: the name and the tag on the row and, when the row is open, the
# details and buttons under it ($null and none when it is closed). Wrap the call in @( ).
function Read-Skills([IntPtr]$Window) {
    for ($j = 0; ; $j++) {
        $base = $UiId.Skill + 8 * $j
        $row = Read-Ui $Window $UiPanel.Page $base
        if ($null -eq $row) { break }
        $two = $row -split "`n", 2
        $buttons = @(foreach ($k in 2..7) {
                $t = Read-Ui $Window $UiPanel.Page ($base + $k)
                if ($null -ne $t) { [pscustomobject]@{ Id = $base + $k; Text = $t } }
            })
        [pscustomobject]@{
            Index = $j; Id = $base; Name = $two[0]; Tag = $(if ($two.Count -gt 1) { $two[1] } else { '' })
            Detail = (Read-Ui $Window $UiPanel.Page ($base + 1)); Buttons = $buttons
        }
    }
}

# Shows the page of the repository at this place in the list, and waits until it shows.
function Open-Repo([IntPtr]$Window, [int]$Index = 0) {
    Click-Ui $Window $UiPanel.Nav ($UiId.NavRepo + $Index)
    Wait-Live { if ((Find-Ui $Window $UiPanel.Page $UiId.Title) -ne [IntPtr]::Zero) { $true } } 'the page of the repository' 10 100
    Start-Sleep -Milliseconds 300
}

# A question the app shows (adding or removing a repository, for example), or zero.
function Find-Ask([Diagnostics.Process]$P) {
    Find-LiveWindow $P.Id $ASK_CLASS -Visible
}

# A key sent to one control of a window, through its message queue (the keyboard itself is not used, so it does not
# matter which window the person at the computer is typing in).
function Send-Key([IntPtr]$Control, [int]$Key) {
    [void][TssLive]::PostMessageW($Control, $WM_KEYDOWN, [IntPtr]$Key, [IntPtr]::Zero)
}

# The children of a window or panel in tab order (the window keeps the z-order in the order it lays them out), each
# with where it is (client pixels) and how it is.
function Get-Layout([IntPtr]$Window) {
    foreach ($line in [TssLive]::Children($Window)) {
        $part = $line -split '\|'
        $box = $part[5] -split ','
        [pscustomobject]@{
            Id = [int]$part[0]; Class = $part[1]; Tab = ($part[2] -eq 'tab'); Shown = ($part[3] -eq 'shown'); On = ($part[4] -eq 'on')
            X = [int]$box[0]; Y = [int]$box[1]; W = [int]$box[2]; H = [int]$box[3]
            Style = [Convert]::ToInt64($part[6], 16); Handle = [IntPtr][int64]$part[7]
        }
    }
}

# A pop-up menu of the app that is showing, once it has been put in its place on screen; zero when none shows in time.
function Wait-Menu([Diagnostics.Process]$P, [int]$TimeoutSec = 5) {
    # The window is made before the menu is put in its place, and is 100 by 100 at the corner of the screen until then.
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        $m = Find-LiveWindow $P.Id $MENU_CLASS -Visible
        if ($m -ne [IntPtr]::Zero) {
            $b = [TssLive]::Bounds($m)
            if ($b -ne '0,0,100,100') { Start-Sleep -Milliseconds 400; return $m }
        }
        Start-Sleep -Milliseconds 100
    }
    [IntPtr]::Zero
}

# Chooses an item of a pop-up menu that is showing, as the keyboard does: the item is selected, then Enter is pressed.
function Choose-MenuItem([IntPtr]$Menu, [int]$Index) {
    [void][TssLive]::SendMessageW($Menu, $MN_SELECTITEM, [IntPtr]$Index, [IntPtr]::Zero)
    [void][TssLive]::PostMessageW($Menu, $WM_KEYDOWN, [IntPtr]$VK_RETURN, [IntPtr]::Zero)
}

# The pop-up menu of the tray icon, shown the way a right click on the icon shows it (the icon sits in the overflow
# of this machine's taskbar, so the click is sent to the app's window). Returns the menu's window once it has been
# placed on screen, or zero.
function Show-TrayMenu([Diagnostics.Process]$P) {
    $tray = Find-LiveWindow $P.Id $TRAY_CLASS
    $r = [TssLive]::TrayIconRect($tray, [uint32]$TRAY_ICON_ID)
    $x = 1000; $y = 1000
    if ($r) { $x = [int](($r[0] + $r[2]) / 2); $y = [int](($r[1] + $r[3]) / 2) }
    [void][TssLive]::PostMessageW($tray, $TRAY_CALLBACK, [IntPtr](($y -shl 16) -bor ($x -band 0xFFFF)), [IntPtr]$WM_CONTEXTMENU)
    Wait-Menu $P
}

# Closes a pop-up menu that is showing, as clicking elsewhere does.
function Hide-TrayMenu([Diagnostics.Process]$P) {
    $tray = Find-LiveWindow $P.Id $TRAY_CLASS
    if ($tray -ne [IntPtr]::Zero) { [void][TssLive]::PostMessageW($tray, $WM_CANCELMODE, [IntPtr]::Zero, [IntPtr]::Zero) }
}
