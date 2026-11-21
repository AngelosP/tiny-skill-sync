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
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool AttachThreadInput(uint from, uint to, bool attach);
    [DllImport("user32.dll")] static extern bool BringWindowToTop(IntPtr hwnd);
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] static extern bool GetGUIThreadInfo(uint thread, ref GUITHREADINFO info);

    // Windows ignores SetForegroundWindow from a process that did not get the last input, for example once a window
    // the test opened and closed has handed the foreground to another app. Sharing the foreground thread's input
    // state for the call lets it through. True when the window is in the foreground afterwards.
    public static bool Foreground(IntPtr window) {
        if (SetForegroundWindow(window) && GetForegroundWindow() == window) return true;
        uint pid;
        uint me = GetCurrentThreadId();
        IntPtr front = GetForegroundWindow();
        uint frontThread = front == IntPtr.Zero ? 0 : GetWindowThreadProcessId(front, out pid);
        bool attached = frontThread != 0 && frontThread != me && AttachThreadInput(me, frontThread, true);
        try {
            BringWindowToTop(window);
            SetForegroundWindow(window);
        } finally {
            if (attached) AttachThreadInput(me, frontThread, false);
        }
        return GetForegroundWindow() == window;
    }
    [StructLayout(LayoutKind.Sequential)] struct GUITHREADINFO {
        public uint Size, Flags;
        public IntPtr Active, Focus, Capture, MenuOwner, MoveSize, Caret;
        public RECT CaretRect;
    }

    public static IntPtr Focused(IntPtr window) {
        uint pid;
        uint thread = GetWindowThreadProcessId(window, out pid);
        GUITHREADINFO info = new GUITHREADINFO();
        info.Size = (uint)Marshal.SizeOf(typeof(GUITHREADINFO));
        return thread != 0 && GetGUIThreadInfo(thread, ref info) ? info.Focus : IntPtr.Zero;
    }
    [DllImport("user32.dll")] public static extern int GetScrollPos(IntPtr hwnd, int bar);
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

    [DllImport("user32.dll")] static extern bool RedrawWindow(IntPtr hwnd, IntPtr rect, IntPtr region, uint flags);

    // Makes one control paint itself again at once, and nothing else: what a control does when the mouse comes onto it
    // or it is clicked (control.rs, invalidate). RDW_INVALIDATE | RDW_UPDATENOW.
    public static bool Repaint(IntPtr hwnd) { return RedrawWindow(hwnd, IntPtr.Zero, IntPtr.Zero, 0x0001 | 0x0100); }

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

    // How many pixels of a part differ between two pictures of the same window by more than 16 in any one colour, or
    // -1 when the pictures are not the same size.
    public static int Differ(string pathA, string pathB, int x, int y, int w, int h) {
        using (Bitmap a = new Bitmap(pathA))
        using (Bitmap b = new Bitmap(pathB)) {
            if (a.Width != b.Width || a.Height != b.Height) return -1;
            int n = 0;
            for (int j = Math.Max(0, y); j < Math.Min(a.Height, y + h); j++) {
                for (int i = Math.Max(0, x); i < Math.Min(a.Width, x + w); i++) {
                    Color p = a.GetPixel(i, j), q = b.GetPixel(i, j);
                    if (Math.Abs(p.R - q.R) > 16 || Math.Abs(p.G - q.G) > 16 || Math.Abs(p.B - q.B) > 16) n++;
                }
            }
            return n;
        }
    }

    // Find the glyphs that appeared on a row, not its hover background, unchanged name or status. Use the
    // middle band to exclude the focus outline. Return x centres relative to the row for pointer hit tests.
    public static int[] NewGlyphs(string before, string after, int x, int y, int w, int h, int gap) {
        var centres = new System.Collections.Generic.List<int>();
        using (Bitmap a = new Bitmap(before))
        using (Bitmap b = new Bitmap(after)) {
            if (a.Size != b.Size || x < 0 || y < 0 || x+w > a.Width || y+h > a.Height)
                return centres.ToArray();
            int first = -1, last = -1;
            for (int i = gap; i < w - gap; i++) {
                if (ColumnInk(b, x+i, y+h/4, y+3*h/4) && !ColumnInk(a, x+i, y+h/4, y+3*h/4)) {
                    if (first < 0 || i-last > gap) {
                        if (first >= 0) centres.Add((first+last)/2);
                        first = i;
                    }
                    last = i;
                }
            }
            if (first >= 0) centres.Add((first+last)/2);
        }
        return centres.ToArray();
    }

    static bool ColumnInk(Bitmap image, int x, int top, int bottom) {
        Color bg = image.GetPixel(x, top);
        for (int y = top+1; y <= bottom; y++) {
            Color p = image.GetPixel(x, y);
            if (Math.Max(Math.Abs(p.R-bg.R), Math.Max(Math.Abs(p.G-bg.G), Math.Abs(p.B-bg.B))) > 48) return true;
        }
        return false;
    }

    [DllImport("user32.dll")] static extern bool OpenClipboard(IntPtr owner);
    [DllImport("user32.dll")] static extern bool EmptyClipboard();
    [DllImport("user32.dll")] static extern bool CloseClipboard();

    // Empties the clipboard: how the ui scenario puts back one that held no text. Another program can have it open for
    // a moment, so this tries a few times.
    public static bool ClearClipboard() {
        for (int i = 0; i < 10; i++) {
            if (OpenClipboard(IntPtr.Zero)) {
                bool ok = EmptyClipboard();
                CloseClipboard();
                return ok;
            }
            System.Threading.Thread.Sleep(20);
        }
        return false;
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
    public static string Text(IntPtr hwnd) { return Text(hwnd, 256); }
    public static string Text(IntPtr hwnd, int minimum) {
        // A visible native tooltip can supply WM_GETTEXT even when WM_GETTEXTLENGTH has no stored caption.
        int n = Math.Max(minimum, (int)SendMessageW(hwnd, 0x000E, IntPtr.Zero, IntPtr.Zero));
        StringBuilder sb = new StringBuilder(n + 2);
        SendMessageW(hwnd, 0x000D, (IntPtr)(n + 1), sb);
        return sb.ToString();
    }

    // The tag the app gives an app folder (config.rs, instance_tag): its mutex and, for a copy started with TSS_HOME, its
    // startup entry's key are named by it.
    public static string InstanceTag(string home) {
        ulong hash = 0xcbf29ce484222325UL;
        foreach (byte b in Encoding.UTF8.GetBytes(home.ToLowerInvariant())) {
            unchecked { hash = (hash ^ b) * 0x100000001b3UL; }
        }
        return hash.ToString("x16");
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern uint GetShortPathNameW(string path, StringBuilder buf, uint size);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern uint GetLongPathNameW(string path, StringBuilder buf, uint size);

    // A path that exists in its short (8.3) or long form, at any length, without \\?\; null when Windows does not say.
    public static string ShortPath(string path) { return PathForm(true, path); }
    public static string LongPath(string path) { return PathForm(false, path); }

    static string PathForm(bool shortForm, string path) {
        string given = path.StartsWith(@"\\?\") ? path : @"\\?\" + path;
        StringBuilder sb = new StringBuilder(32768);
        uint n = shortForm ? GetShortPathNameW(given, sb, (uint)sb.Capacity) : GetLongPathNameW(given, sb, (uint)sb.Capacity);
        if (n == 0 || n >= sb.Capacity) return null;
        string s = sb.ToString();
        return s.StartsWith(@"\\?\") ? s.Substring(4) : s;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO {
        public int cb; public string lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2; public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string app, StringBuilder command, IntPtr procAttr, IntPtr threadAttr, bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] public static extern uint GetProcessId(IntPtr process);
    [DllImport("kernel32.dll")] public static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr process, out uint code);

    // Starts a command line as a startup entry is started: the whole line as it is written, no program named apart from
    // it, with this environment ("NAME=value" each) and working folder. Returns the process's handle (CloseHandle it).
    public static IntPtr StartCommand(string command, string[] env, string cwd) {
        string[] sorted = (string[])env.Clone();
        Array.Sort(sorted, StringComparer.OrdinalIgnoreCase);
        IntPtr block = Marshal.StringToHGlobalUni(string.Join("\0", sorted) + "\0\0");
        try {
            STARTUPINFO si = new STARTUPINFO();
            si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
            PROCESS_INFORMATION pi;
            if (!CreateProcessW(null, new StringBuilder(command, command.Length + 1), IntPtr.Zero, IntPtr.Zero, false, 0x400, block, cwd, ref si, out pi)) {
                throw new System.ComponentModel.Win32Exception();
            }
            CloseHandle(pi.hThread);
            return pi.hProcess;
        } finally { Marshal.FreeHGlobal(block); }
    }

    // A process's exit code once it has ended, waiting for it up to this long; -1 while it is still running.
    public static long Exited(IntPtr process, int waitMs) {
        if (WaitForSingleObject(process, (uint)waitMs) != 0) return -1;
        uint code;
        if (!GetExitCodeProcess(process, out code)) return -1;
        return code;
    }
}
'@

[void][TssLive]::SetProcessDPIAware()

$WM_COMMAND = 0x111
$WM_CLOSE = 0x10
$WM_SETTEXT = 0xC
$WM_KEYDOWN = 0x100
$WM_LBUTTONDOWN = 0x201
$WM_LBUTTONUP = 0x202
$WM_CANCELMODE = 0x1F
$WM_EXITSIZEMOVE = 0x232
$BM_CLICK = 0xF5
$BM_GETCHECK = 0xF0
$MN_SELECTITEM = 0x1E5
$CBN_SELENDOK = 9
$VK_ESCAPE = 0x1B
$VK_RETURN = 0x0D
$VK_APPS = 0x5D
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
    ConfirmSource = 315; CancelSource = 316; RetrySource = 317; EditSource = 318
    DefaultEvery = 320; OpenSkills = 321; OpenBackups = 322; Startup = 325; StartupText = 413
    AppHeading = 420; AppStatus = 421; AppCheck = 422; AppInstall = 423; AppAuto = 424; AppEvery = 425; AppWhen = 426
    Title = 400; Private = 401; Sub = 402; InfoLead = 403; InfoText = 404; SkillsDir = 410; TokenText = 411; Empty = 412
    Back = 330; DiffAction = 333; Note = 100000; File = 200000; Gap = 300000
    # Plus 16 per skill: 0 row ("name`ntag"), 1 explanation, 8 facts; 2 Show changes, 4 to 7 repair choices.
    # Everyday actions are drawn inside the row, not separate child windows.
    Skill = 1000000; SkillStride = 16
}
# A question (src\win\ui\page.rs, mod ask_id): its controls are on one panel.
$AskPanel = 100
$AskId = @{ Ok = 1; Cancel = 2; Edit = 10; Heading = 11; Text = 12; Label = 13; Hint = 14; ComboLabel = 15; Combo = 16; Note = 17; Choice = 20 }

# ---------------------------------------------------------------- a throwaway install

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

# Override both Copilot directories in the child environment, never the caller's environment.
function Get-LiveCopilotExe {
    $exe = $env:TSS_COPILOT_CLI
    if ($null -eq $exe -and $env:LOCALAPPDATA) { $exe = Join-Path $env:LOCALAPPDATA 'GitHubCopilotCLI\copilot.exe' }
    if ($exe -and $exe -match '^[A-Za-z]:\\|^\\\\[^?\\]' -and $exe -like '*.exe' -and (Test-Path -LiteralPath $exe -PathType Leaf)) {
        [IO.Path]::GetFullPath($exe)
    }
}

function Assert-LiveCopilotEnv([string]$Root, $Env) {
    if (-not $env:USERPROFILE -or -not $env:LOCALAPPDATA) { throw 'Cannot identify both real Copilot folders.' }
    $full = { param($p) ([IO.Path]::GetFullPath($p)).TrimEnd('\') + '\' }
    $within = { param($p, $parent) $p.StartsWith($parent, [StringComparison]::OrdinalIgnoreCase) }
    $base = & $full $Root
    $protected = @((& $full (Join-Path $env:USERPROFILE '.copilot')), (& $full (Join-Path $env:LOCALAPPDATA 'copilot')))
    foreach ($key in 'COPILOT_HOME', 'COPILOT_CACHE_HOME') {
        if (-not $Env[$key] -or $Env[$key] -notmatch '^[A-Za-z]:\\|^\\\\[^?\\]') { throw "$key must be a full throwaway path." }
        $dir = & $full $Env[$key]
        if ($dir -eq $base -or -not (& $within $dir $base)) { throw "$key is not inside the scenario folder." }
        foreach ($real in $protected) {
            if ((& $within $dir $real) -or (& $within $real $dir)) { throw "$key overlaps a real Copilot folder." }
        }
        # Check parents first so even looking up a child cannot follow a junction.
        $parts = @()
        for ($entry = $dir.TrimEnd('\'); $entry; $entry = Split-Path -Parent $entry) { $parts = @($entry) + $parts }
        foreach ($part in $parts) {
            $item = Get-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
            if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "$key is reached through a link." }
        }
    }
    if ((& $full $Env['COPILOT_HOME']) -eq (& $full $Env['COPILOT_CACHE_HOME']) -or $Env['COPILOT_AUTO_UPDATE'] -ne 'false') {
        throw 'Copilot needs separate throwaway folders and automatic updates off.'
    }
}

function Get-LivePluginList([string]$Root, [hashtable]$Env) {
    $psi = New-LiveStartInfo $Root 'copilot.exe' @('plugin', 'list', '--json') $Env
    $psi.FileName = $Env.TSS_COPILOT_CLI
    $psi.CreateNoWindow = $true
    Assert-LiveCopilotEnv $Root $psi.EnvironmentVariables
    $r = Invoke-LiveConsole $psi 'copilot plugin list --json' 30
    if ($r.Code -ne 0) { throw "Isolated copilot plugin list failed (exit $($r.Code))." }
    try { $plugins = ConvertFrom-Json -InputObject $r.Out } catch { throw 'Copilot did not return a readable plugin list.' }
    foreach ($plugin in $plugins) { $plugin }
}

# Starts the tray app on the throwaway install.
function Start-LiveTray($Root, [string[]]$AppArgs = @(), [hashtable]$Env = @{}) {
    $psi = New-LiveStartInfo $Root 'tiny-skill-sync.exe' $AppArgs $Env
    [Diagnostics.Process]::Start($psi)
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
        # Windows PowerShell cannot delete a path longer than 260 characters (the signin scenario makes some); the \\?\
        # form can.
        if (Test-Path $Root) { try { [IO.Directory]::Delete('\\?\' + $Root, $true) } catch { } }
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

# What a public repository's folder lists for anyone without a token, asked exactly as the app asks it. GitHub's servers
# may keep that answer for up to a minute after a push, so a scenario whose repository is read without a token waits
# for this one, not for the signed-in listing. Paths are relative to the folder. At most a few calls: the limit
# without a token is 60 an hour.
function Get-PublicFiles([string]$Repo, [string]$Ref, [string]$Path) {
    $spec = [uri]::EscapeDataString($(if ($Path) { "${Ref}:${Path}" } else { $Ref }))
    $headers = @{ 'Accept' = 'application/vnd.github+json'; 'X-GitHub-Api-Version' = '2022-11-28'; 'User-Agent' = 'tss-e2e' }
    $tree = Invoke-RestMethod -UseBasicParsing -Headers $headers -Uri "https://api.github.com/repos/$Repo/git/trees/${spec}?recursive=1"
    if ($tree.truncated) { throw 'the listing is truncated' }
    $map = [ordered]@{}
    foreach ($e in $tree.tree) { if ($e.type -eq 'blob') { $map[[string]$e.path] = $e.sha } }
    $map
}

# The commit a public branch points to for anyone without a token, asked exactly as the app asks it (it may lag the
# listing above, since GitHub's servers keep each answer on its own).
function Get-PublicCommit([string]$Repo, [string]$Ref) {
    $headers = @{ 'Accept' = 'application/vnd.github.sha'; 'X-GitHub-Api-Version' = '2022-11-28'; 'User-Agent' = 'tss-e2e' }
    $r = Invoke-WebRequest -UseBasicParsing -Headers $headers -Uri "https://api.github.com/repos/$Repo/commits/$([uri]::EscapeDataString($Ref))"
    [Text.Encoding]::ASCII.GetString($r.RawContentStream.ToArray()).Trim().ToLowerInvariant()
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
    if ($h -eq [IntPtr]::Zero) {
        [void](Wait-Live { (Find-LiveWindow $P.Id $TRAY_CLASS) -ne [IntPtr]::Zero } 'the tray window to start' 15 100)
        Send-LiveCommand $P $CMD_SETTINGS
    }
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

# The skills on a repository's page, top to bottom. Only repair buttons depend on disclosure; collect all
# child buttons (including obsolete everyday ids) so an unexpected extra button is observable.
function Read-Skills([IntPtr]$Window) {
    for ($j = 0; ; $j++) {
        $base = $UiId.Skill + $UiId.SkillStride * $j
        $row = Read-Ui $Window $UiPanel.Page $base
        if ($null -eq $row) { break }
        $parts = $row -split '\r?\n', 4 # Keep empty fields and all remaining lines of explanation.
        if ($parts.Count -ne 4) { throw "Skill row $base is missing native text fields." }
        $buttons = @(foreach ($k in @(2..7) + @(9..12)) {
                $t = Read-Ui $Window $UiPanel.Page ($base + $k)
                if ($null -ne $t) { [pscustomobject]@{ Id = $base + $k; Text = $t } }
            })
        [pscustomobject]@{
            Index = $j; Id = $base; Name = $parts[0]; Tag = $parts[1]
            Facts = $parts[2]; Detail = $parts[3]; Buttons = $buttons
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

# ---------------------------------------------------------------- Start with Windows

# The entry's name (autostart::VALUE_NAME), and Windows' own keys under HKEY_CURRENT_USER, which a script only reads: to
# prove, before and after, that a scenario did not change them. A copy started with TSS_HOME uses keys of its own
# under $TestKeyRoot (autostart::TEST_ROOT), which scripts\dev.ps1 clean deletes.
$StartupValue = 'Tiny Skill Sync'
$RealRunKey = 'Software\Microsoft\Windows\CurrentVersion\Run'
$RealApprovedKey = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
$TestKeyRoot = 'Software\tiny-skill-sync-test'

# A value under HKEY_CURRENT_USER as it is stored (variables not expanded, bytes as "hex:..."), or $null.
function Read-UserValue([string]$Key, [string]$Name) {
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($Key, $false)
    if ($null -eq $k) { return $null }
    try {
        $v = $k.GetValue($Name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($v -is [byte[]]) { return 'hex:' + ([BitConverter]::ToString($v) -replace '-', '') }
        $v
    } finally { $k.Close() }
}

# What Windows' own keys say about the app, to compare before and after a scenario.
function Get-RealStartup {
    "Run: $(Read-UserValue $RealRunKey $StartupValue); StartupApproved: $(Read-UserValue $RealApprovedKey $StartupValue)"
}

# The keys a copy started with this app folder (TSS_HOME) uses instead of Windows' own (autostart::Keys::choose).
function Get-StartupKeys([string]$AppHome) {
    $root = "$TestKeyRoot\$([TssLive]::InstanceTag($AppHome))"
    [pscustomobject]@{ Root = $root; Run = "$root\Run"; Approved = "$root\StartupApproved\Run" }
}

function Read-StartupEntry($Keys) { Read-UserValue $Keys.Run $StartupValue }

function Read-StartupMark($Keys) { Read-UserValue $Keys.Approved $StartupValue }

function Assert-TestKey([string]$Key) {
    if (-not $Key.StartsWith("$TestKeyRoot\", [StringComparison]::OrdinalIgnoreCase)) { throw "'$Key' is not a key of the scripts; refusing to change it." }
}

# Switches the entry off the way Task Manager's Startup apps page does: a mark whose first byte is odd, then when.
function Set-StartupMark($Keys) {
    Assert-TestKey $Keys.Approved
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($Keys.Approved)
    try {
        $mark = [byte[]](@(3, 0, 0, 0) + [BitConverter]::GetBytes([DateTime]::UtcNow.ToFileTimeUtc()))
        $k.SetValue($StartupValue, $mark, [Microsoft.Win32.RegistryValueKind]::Binary)
    } finally { $k.Close() }
}

function Remove-StartupKeys($Keys) {
    Assert-TestKey $Keys.Root
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($Keys.Root, $false)
}

# Starts an entry's command the way Windows does when the user signs in: the command line exactly as written, from
# System32. The environment is this one with the changes given ($null removes a variable), and it must give the app a
# throwaway app folder and skills folder. Returns the process's handle and id: Get-EntryProcess for one that keeps
# running, or [TssLive]::Exited and CloseHandle.
function Start-Entry([string]$Command, [hashtable]$Env) {
    $vars = @{}
    $now = [Environment]::GetEnvironmentVariables()
    foreach ($k in $now.Keys) { $vars[[string]$k] = [string]$now[$k] }
    foreach ($k in $Env.Keys) { if ($null -eq $Env[$k]) { $vars.Remove($k) } else { $vars[$k] = [string]$Env[$k] } }
    $vars['TSS_NO_APP_UPDATES'] = '1'
    $vars['TSS_NO_SAVED_TOKEN'] = '1'
    foreach ($k in 'TSS_HOME', 'TSS_SKILLS_DIR') {
        if (-not $vars[$k] -or -not ([string]$vars[$k]).StartsWith("$TempRoot\tss-", [StringComparison]::OrdinalIgnoreCase)) {
            throw "refusing to start $Command`: $k is not a throwaway folder"
        }
    }
    $block = [string[]]@($vars.Keys | ForEach-Object { "$_=$($vars[$_])" })
    $h = [TssLive]::StartCommand($Command, $block, (Join-Path $env:SystemRoot 'System32'))
    [pscustomobject]@{ Handle = $h; Id = [int][TssLive]::GetProcessId($h) }
}

# The process a start from an entry made, as a Process with a handle of its own (the start's handle is closed).
function Get-EntryProcess($Started) {
    try {
        $code = [TssLive]::Exited($Started.Handle, 0)
        if ($code -ge 0) { throw "the app started from the entry ended at once, with exit code $code" }
        $p = [Diagnostics.Process]::GetProcessById($Started.Id)
        [void]$p.Handle
        $p
    } finally { [void][TssLive]::CloseHandle($Started.Handle) }
}
