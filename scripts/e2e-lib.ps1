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

    [DllImport("user32.dll")] static extern bool GetUpdateRect(IntPtr hwnd, out RECT rect, bool erase);
    [DllImport("user32.dll")] static extern IntPtr WindowFromPoint(POINT point);
    [DllImport("user32.dll")] static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);

    // What the window still has to paint: "window" if the window itself has, and the id of each control that has.
    public static string Pending(IntPtr parent) {
        StringBuilder sb = new StringBuilder();
        RECT r;
        if (GetUpdateRect(parent, out r, false)) sb.Append("window ");
        for (IntPtr h = GetWindow(parent, 5); h != IntPtr.Zero; h = GetWindow(h, 2)) {
            if (GetUpdateRect(h, out r, false)) sb.Append(GetDlgCtrlID(h) + " ");
        }
        return sb.ToString().Trim();
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
    [DllImport("user32.dll")] static extern bool IsWindowEnabled(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
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

    // A key pressed and let go, for the keyboard, but only while a window of this process has the keyboard: whatever
    // else has it must not get the key. Says whether it was pressed.
    public static bool KeyToProcess(uint pid, byte vk) {
        uint owner;
        GetWindowThreadProcessId(GetForegroundWindow(), out owner);
        if (owner != pid) return false;
        keybd_event(vk, 0, 0, UIntPtr.Zero);
        keybd_event(vk, 0, 2, UIntPtr.Zero);
        return true;
    }

    // The first child of this class whose top-left corner is at x,y (in 96 DPI pixels, give or take 2) in the parent's
    // client area: a label has no id to ask for.
    public static IntPtr ChildAt(IntPtr parent, string cls, int x, int y) {
        int dpi = (int)GetDpiForWindow(parent);
        if (dpi <= 0) dpi = 96;
        x = (x * dpi + 48) / 96;
        y = (y * dpi + 48) / 96;
        POINT origin = new POINT();
        ClientToScreen(parent, ref origin);
        for (IntPtr h = GetWindow(parent, 5); h != IntPtr.Zero; h = GetWindow(h, 2)) {
            StringBuilder sb = new StringBuilder(64);
            GetClassNameW(h, sb, 64);
            RECT r;
            GetWindowRect(h, out r);
            if (string.Equals(sb.ToString(), cls, StringComparison.OrdinalIgnoreCase) && Math.Abs(r.Left - origin.X - x) <= 2 && Math.Abs(r.Top - origin.Y - y) <= 2) return h;
        }
        return IntPtr.Zero;
    }

    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
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

    // A part of the screen as a PNG, each pixel made zoom times as large so that a small icon can be seen.
    public static string CaptureArea(int left, int top, int width, int height, int zoom, string path) {
        using (Bitmap raw = new Bitmap(width, height))
        using (Graphics g = Graphics.FromImage(raw)) {
            g.CopyFromScreen(left, top, 0, 0, new Size(width, height));
            using (Bitmap big = new Bitmap(width * zoom, height * zoom))
            using (Graphics gb = Graphics.FromImage(big)) {
                gb.InterpolationMode = System.Drawing.Drawing2D.InterpolationMode.NearestNeighbor;
                gb.PixelOffsetMode = System.Drawing.Drawing2D.PixelOffsetMode.Half;
                gb.DrawImage(raw, new Rectangle(0, 0, width * zoom, height * zoom), new Rectangle(0, 0, width, height), GraphicsUnit.Pixel);
                big.Save(path, ImageFormat.Png);
            }
        }
        return width + "x" + height;
    }

    [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr parent, int id);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr SendMessageW(IntPtr hwnd, uint msg, IntPtr wp, StringBuilder lp);
    [DllImport("kernel32.dll")] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] static extern IntPtr VirtualAllocEx(IntPtr process, IntPtr address, UIntPtr size, uint type, uint protect);
    [DllImport("kernel32.dll")] static extern bool VirtualFreeEx(IntPtr process, IntPtr address, UIntPtr size, uint type);
    [DllImport("kernel32.dll")] static extern bool WriteProcessMemory(IntPtr process, IntPtr address, byte[] buffer, UIntPtr size, out UIntPtr written);
    [DllImport("kernel32.dll")] static extern bool ReadProcessMemory(IntPtr process, IntPtr address, byte[] buffer, UIntPtr size, out UIntPtr read);

    [StructLayout(LayoutKind.Sequential)]
    struct LVITEM {
        public uint mask; public int iItem; public int iSubItem; public uint state; public uint stateMask;
        public IntPtr pszText; public int cchTextMax; public int iImage; public IntPtr lParam;
        public int iIndent; public int iGroupId; public uint cColumns; public IntPtr puColumns; public IntPtr piColFmt; public int iGroup;
    }

    // The text of a control of any kind (button, edit, static, combo box).
    public static string Text(IntPtr hwnd) {
        int n = (int)SendMessageW(hwnd, 0x000E, IntPtr.Zero, IntPtr.Zero);
        StringBuilder sb = new StringBuilder(n + 2);
        SendMessageW(hwnd, 0x000D, (IntPtr)(n + 1), sb);
        return sb.ToString();
    }

    public static int ListCount(IntPtr list) { return (int)SendMessageW(list, 0x1004, IntPtr.Zero, IntPtr.Zero); }

    // A list view lives in another process, so the item and its text are exchanged through memory in that process.
    static object Remote(IntPtr list, Func<IntPtr, IntPtr, object> body) {
        uint pid;
        GetWindowThreadProcessId(list, out pid);
        IntPtr proc = OpenProcess(0x0008 | 0x0010 | 0x0020 | 0x0400, false, pid);
        if (proc == IntPtr.Zero) throw new Exception("cannot open the process of the list");
        IntPtr mem = VirtualAllocEx(proc, IntPtr.Zero, (UIntPtr)4096, 0x3000, 0x04);
        try { return body(proc, mem); }
        finally { VirtualFreeEx(proc, mem, UIntPtr.Zero, 0x8000); CloseHandle(proc); }
    }

    static void Put(IntPtr proc, IntPtr at, byte[] bytes) {
        UIntPtr done;
        if (!WriteProcessMemory(proc, at, bytes, (UIntPtr)bytes.Length, out done)) throw new Exception("WriteProcessMemory failed");
    }

    static byte[] Get(IntPtr proc, IntPtr at, int size) {
        byte[] bytes = new byte[size];
        UIntPtr done;
        if (!ReadProcessMemory(proc, at, bytes, (UIntPtr)size, out done)) throw new Exception("ReadProcessMemory failed");
        return bytes;
    }

    static byte[] Bytes(LVITEM item) {
        int size = Marshal.SizeOf(typeof(LVITEM));
        byte[] bytes = new byte[size];
        IntPtr p = Marshal.AllocHGlobal(size);
        try { Marshal.StructureToPtr(item, p, false); Marshal.Copy(p, bytes, 0, size); } finally { Marshal.FreeHGlobal(p); }
        return bytes;
    }

    // The text in one cell of a list view in report mode.
    public static string Cell(IntPtr list, int row, int column) {
        return (string)Remote(list, delegate (IntPtr proc, IntPtr mem) {
            IntPtr text = (IntPtr)(mem.ToInt64() + 256);
            LVITEM item = new LVITEM();
            item.iItem = row; item.iSubItem = column; item.pszText = text; item.cchTextMax = 400;
            Put(proc, mem, Bytes(item));
            int n = (int)SendMessageW(list, 0x1073, (IntPtr)row, mem);
            byte[] raw = Get(proc, text, Math.Max(0, Math.Min(n, 399)) * 2);
            return Encoding.Unicode.GetString(raw);
        });
    }

    // Selects a row the way a click would (the window is told, so it reacts as it does to a person).
    public static void Select(IntPtr list, int row) {
        Remote(list, delegate (IntPtr proc, IntPtr mem) {
            LVITEM item = new LVITEM();
            item.stateMask = 3; item.state = 3;
            Put(proc, mem, Bytes(item));
            SendMessageW(list, 0x102B, (IntPtr)row, mem);
            return null;
        });
    }

    // Deselects a row the way Ctrl+click on the selected row does.
    public static void Deselect(IntPtr list, int row) {
        Remote(list, delegate (IntPtr proc, IntPtr mem) {
            LVITEM item = new LVITEM();
            item.stateMask = 3; item.state = 0;
            Put(proc, mem, Bytes(item));
            SendMessageW(list, 0x102B, (IntPtr)row, mem);
            return null;
        });
    }

    [DllImport("user32.dll")] static extern IntPtr GetDC(IntPtr hwnd);
    [DllImport("user32.dll")] static extern int ReleaseDC(IntPtr hwnd, IntPtr dc);
    [DllImport("gdi32.dll")] static extern IntPtr SelectObject(IntPtr dc, IntPtr obj);
    [DllImport("gdi32.dll", CharSet = CharSet.Unicode)] static extern bool GetTextExtentPoint32W(IntPtr dc, string text, int count, out SIZE size);
    [StructLayout(LayoutKind.Sequential)] struct SIZE { public int cx, cy; }

    // How wide the control's text is when written on one line in the control's own font, and how wide the control is:
    // "needed,available" in pixels. A label whose text needs more than it has ends in "...".
    public static string OneLine(IntPtr hwnd) { return OneLine(hwnd, Text(hwnd)); }

    // The same for a text that is to be shown in the control.
    public static string OneLine(IntPtr hwnd, string text) {
        IntPtr font = SendMessageW(hwnd, 0x0031, IntPtr.Zero, IntPtr.Zero);
        IntPtr dc = GetDC(hwnd);
        try {
            IntPtr old = SelectObject(dc, font);
            SIZE size;
            GetTextExtentPoint32W(dc, text, text.Length, out size);
            SelectObject(dc, old);
            RECT r;
            GetClientRect(hwnd, out r);
            return size.cx + "," + (r.Right - r.Left);
        } finally { ReleaseDC(hwnd, dc); }
    }
}
'@

[void][TssLive]::SetProcessDPIAware()

$WM_COMMAND = 0x111
$WM_CLOSE = 0x10
$WM_KEYDOWN = 0x100
$WM_CANCELMODE = 0x1F
$BM_CLICK = 0xF5
$VK_ESCAPE = 0x1B
$CMD_SETTINGS = 1001
$CMD_CHECK = 1002
$CMD_QUIT = 1003
$TRAY_CLASS = 'TinySkillSyncTray'
$SETTINGS_CLASS = 'TinySkillSyncSettings'
$ASK_CLASS = 'TinySkillSyncAsk'
$MENU_CLASS = '#32768'
$TRAY_ICON_ID = 1
$TRAY_CALLBACK = 0x8001
$WM_CONTEXTMENU = 0x7B

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
    foreach ($key in @($Env.Keys)) { $psi.EnvironmentVariables[$key] = [string]$Env[$key] }
    $psi
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

# The tray app's "Check now": asks for it the way the menu does and waits until that job has finished. Returns how
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

# ---------------------------------------------------------------- the settings window, read from outside

function Open-Settings([Diagnostics.Process]$P) {
    $h = Find-LiveWindow $P.Id $SETTINGS_CLASS -Visible
    if ($h -eq [IntPtr]::Zero) { Send-LiveCommand $P $CMD_SETTINGS }
    Wait-Live { $w = Find-LiveWindow $P.Id $SETTINGS_CLASS -Visible; if ($w -ne [IntPtr]::Zero) { $w } } 'the settings window' 15 100
}

function Close-Settings([Diagnostics.Process]$P) {
    $h = Find-LiveWindow $P.Id $SETTINGS_CLASS
    if ($h -ne [IntPtr]::Zero) { [void][TssLive]::PostMessageW($h, $WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero) }
}

function Get-Control([IntPtr]$Window, [int]$Id) {
    $h = [TssLive]::GetDlgItem($Window, $Id)
    if ($h -eq [IntPtr]::Zero) { throw "The window has no control $Id." }
    $h
}

# The rows of a list in the window, each an array of the texts of its columns. Wrap the call in @( ) .
function Read-UiList([IntPtr]$Window, [int]$Id, [int]$Columns) {
    $list = Get-Control $Window $Id
    $n = [TssLive]::ListCount($list)
    for ($i = 0; $i -lt $n; $i++) {
        $row = New-Object string[] $Columns
        for ($col = 0; $col -lt $Columns; $col++) { $row[$col] = [TssLive]::Cell($list, $i, $col) }
        , $row
    }
}

function Read-UiText([IntPtr]$Window, [int]$Id) {
    [TssLive]::Text((Get-Control $Window $Id))
}

# Types into a text box the way a person would (the window gets the text through its own message).
function Set-UiText([IntPtr]$Window, [int]$Id, [string]$Text) {
    $p = [Runtime.InteropServices.Marshal]::StringToHGlobalUni($Text)
    try { [void][TssLive]::SendMessageW((Get-Control $Window $Id), 0x000C, [IntPtr]::Zero, $p) } finally { [Runtime.InteropServices.Marshal]::FreeHGlobal($p) }
}

function Click-Ui([IntPtr]$Window, [int]$Id) {
    [void][TssLive]::PostMessageW($Window, $WM_COMMAND, [IntPtr]$Id, [TssLive]::GetDlgItem($Window, $Id))
}

# Clicks a button or ticks a check box as a person would: the control changes its own look and then tells the window.
function Click-Control([IntPtr]$Window, [int]$Id) {
    [void][TssLive]::PostMessageW((Get-Control $Window $Id), $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
}

# A key sent to one control of a window, through its message queue (the keyboard itself is not used, so it does not
# matter which window the person at the computer is typing in).
function Send-Key([IntPtr]$Control, [int]$Key) {
    [void][TssLive]::PostMessageW($Control, $WM_KEYDOWN, [IntPtr]$Key, [IntPtr]::Zero)
}

# The controls of a window in tab order, each with where it is (client pixels) and how it is.
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

# Whether a button or check box is ticked.
function Get-Ticked([IntPtr]$Window, [int]$Id) {
    ([TssLive]::SendMessageW((Get-Control $Window $Id), 0xF0, [IntPtr]::Zero, [IntPtr]::Zero).ToInt64()) -eq 1
}

# Picks an item of a combo box as a person does: the box shows it, then tells the window that the choice is made.
function Choose-Item([IntPtr]$Window, [int]$Id, [int]$Index) {
    $combo = Get-Control $Window $Id
    [void][TssLive]::SendMessageW($combo, 0x14E, [IntPtr]$Index, [IntPtr]::Zero)
    [void][TssLive]::PostMessageW($Window, $WM_COMMAND, [IntPtr]((9 -shl 16) -bor $Id), $combo)
}

# The status line at the bottom of the settings window: a label has no id to ask for.
function Read-Status([IntPtr]$Window) {
    $h = [TssLive]::ChildAt($Window, 'Static', 12, 551)
    if ($h -eq [IntPtr]::Zero) { return $null }
    [TssLive]::Text($h)
}

# How much room a text needs on one line in the status line of the settings window (its own text by default), and how
# much room the line has, in pixels; the line ends in "..." when the text needs more.
function Get-StatusFit([IntPtr]$Window, [string]$Text = $null) {
    $h = [TssLive]::ChildAt($Window, 'Static', 12, 551)
    if ($h -eq [IntPtr]::Zero) { return $null }
    $part = $(if ($null -eq $Text) { [TssLive]::OneLine($h) } else { [TssLive]::OneLine($h, $Text) }) -split ','
    [pscustomobject]@{ Needed = [int]$part[0]; Available = [int]$part[1] }
}

# How much of a multi-line edit box's text can be seen without scrolling: how many lines the text takes, how many
# fit in the box, and whether the box has a scroll bar for the rest.
function Get-EditFit([IntPtr]$Window, [int]$Id) {
    $edit = Get-Control $Window $Id
    $ask = { param([int]$Message, [int]$Wp) [TssLive]::SendMessageW($edit, $Message, [IntPtr]$Wp, [IntPtr]::Zero).ToInt64() }
    $lines = [int](& $ask 0xBA 0)
    $top = {
        param([int]$Line)
        $at = [int](& $ask 0xBB $Line)
        $y = ((& $ask 0xD6 $at) -shr 16) -band 0xFFFF
        if ($y -ge 0x8000) { $y - 0x10000 } else { $y }
    }
    $height = [int](([TssLive]::Client($edit)) -split ',')[1]
    $visible = $lines
    if ($lines -ge 2) {
        $step = (& $top 1) - (& $top 0)
        if ($step -gt 0) { $visible = [int][math]::Floor($height / $step) }
    }
    $item = Get-Layout $Window | Where-Object { $_.Id -eq $Id } | Select-Object -First 1
    [pscustomobject]@{ Lines = $lines; Visible = $visible; Scrolls = (($item.Style -band 0x200000) -ne 0); Tab = $item.Tab }
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
    # The window is made before the menu is put in its place, and is 100 by 100 at the corner of the screen until then.
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt 5) {
        $m = Find-LiveWindow $P.Id $MENU_CLASS -Visible
        if ($m -ne [IntPtr]::Zero) {
            $b = [TssLive]::Bounds($m)
            if ($b -ne '0,0,100,100') { Start-Sleep -Milliseconds 400; return $m }
        }
        Start-Sleep -Milliseconds 100
    }
    [IntPtr]::Zero
}

# Closes a pop-up menu that is showing, as clicking elsewhere does.
function Hide-TrayMenu([Diagnostics.Process]$P) {
    $tray = Find-LiveWindow $P.Id $TRAY_CLASS
    if ($tray -ne [IntPtr]::Zero) { [void][TssLive]::PostMessageW($tray, $WM_CANCELMODE, [IntPtr]::Zero, [IntPtr]::Zero) }
}
