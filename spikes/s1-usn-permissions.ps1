<#
  Spike S1a - which NTFS change-tracking primitives work for a NON-ELEVATED user?

  Read-only against the volume. Writes only inside a temp folder it creates and deletes.
  Run from a normal (not elevated) Windows PowerShell:   .\s1-usn-permissions.ps1 [-Volume C]
#>
param([string]$Volume = 'C')
$ErrorActionPreference = 'Stop'

$cs = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public class FileSnap
{
    public string Path; public string Error;
    public long Usn = -1; public uint Reason; public long ChangeTime; public long LastWriteTime; public long Size; public string FileId;
}

public static class UsnProbe
{
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr sa, uint disp, uint flags, IntPtr tmpl);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(SafeFileHandle h, uint code, IntPtr inBuf, uint inSize, IntPtr outBuf, uint outSize, out uint ret, IntPtr ov);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetFileInformationByHandleEx(SafeFileHandle h, int cls, IntPtr buf, uint size);

    const uint FSCTL_QUERY_USN_JOURNAL = 0x000900F4;
    const uint FSCTL_READ_USN_JOURNAL = 0x000900BB;
    const uint FSCTL_READ_FILE_USN_DATA = 0x000900EB;
    const uint SHARE_ALL = 7;
    const uint OPEN_EXISTING = 3;
    const uint BACKUP_SEMANTICS = 0x02000000;

    static string Err(int e) { return e + " (" + new Win32Exception(e).Message.Trim() + ")"; }

    // Opens `path` (a volume device like \\.\C: or any file/directory on the volume) with the given access
    // mask and flags, then tries query + read of the change journal through that handle.
    public static string Matrix(string path, uint access, uint flags, string label)
    {
        SafeFileHandle h = CreateFileW(path, access, SHARE_ALL, IntPtr.Zero, OPEN_EXISTING, flags, IntPtr.Zero);
        if (h.IsInvalid) return label + ": open FAILED err=" + Err(Marshal.GetLastWin32Error());
        using (h)
        {
            IntPtr q = Marshal.AllocHGlobal(256);
            IntPtr inb = Marshal.AllocHGlobal(48);
            IntPtr outb = Marshal.AllocHGlobal(65536);
            try
            {
                uint ret;
                string res = label + ": open OK";
                if (!DeviceIoControl(h, FSCTL_QUERY_USN_JOURNAL, IntPtr.Zero, 0, q, 256, out ret, IntPtr.Zero))
                    return res + " | query FAILED err=" + Err(Marshal.GetLastWin32Error());
                ulong journalId = (ulong)Marshal.ReadInt64(q, 0);
                long first = Marshal.ReadInt64(q, 8);
                long next = Marshal.ReadInt64(q, 16);
                res += " | query OK (id=0x" + journalId.ToString("x") + " first=" + first + " next=" + next + ")";

                for (int i = 0; i < 48; i++) Marshal.WriteByte(inb, i, 0);
                Marshal.WriteInt64(inb, 0, next - 8192);
                Marshal.WriteInt32(inb, 8, -1);
                Marshal.WriteInt64(inb, 32, (long)journalId);
                Marshal.WriteInt16(inb, 40, 2);
                Marshal.WriteInt16(inb, 42, 3);
                if (!DeviceIoControl(h, FSCTL_READ_USN_JOURNAL, inb, 48, outb, 65536, out ret, IntPtr.Zero))
                    return res + " | read FAILED err=" + Err(Marshal.GetLastWin32Error());
                int off = 8, records = 0;
                while (off + 8 <= ret) { int len = Marshal.ReadInt32(outb, off); if (len <= 0) break; off += len; records++; }
                return res + " | read OK (" + ret + " bytes, " + records + " records)";
            }
            finally { Marshal.FreeHGlobal(q); Marshal.FreeHGlobal(inb); Marshal.FreeHGlobal(outb); }
        }
    }

    // Journal query through a ROOT DIRECTORY handle (the volume device handle rejects the query for standard users).
    // Returns { journalId, firstUsn, nextUsn, maximumSize } or null.
    public static long[] JournalInfo(string volume)
    {
        SafeFileHandle h = CreateFileW(volume + ":\\", 0x80, SHARE_ALL, IntPtr.Zero, OPEN_EXISTING, BACKUP_SEMANTICS, IntPtr.Zero);
        if (h.IsInvalid) return null;
        using (h)
        {
            IntPtr q = Marshal.AllocHGlobal(256);
            try
            {
                uint ret;
                if (!DeviceIoControl(h, FSCTL_QUERY_USN_JOURNAL, IntPtr.Zero, 0, q, 256, out ret, IntPtr.Zero)) return null;
                return new long[] { Marshal.ReadInt64(q, 0), Marshal.ReadInt64(q, 8), Marshal.ReadInt64(q, 16), Marshal.ReadInt64(q, 40) };
            }
            finally { Marshal.FreeHGlobal(q); }
        }
    }

    public static long NextUsn(string volume)
    {
        long[] j = JournalInfo(volume);
        return j == null ? -1 : j[2];
    }

    // Opens a file or directory with only FILE_READ_ATTRIBUTES and collects change-tracking metadata.
    public static FileSnap Snap(string path)
    {
        FileSnap s = new FileSnap(); s.Path = path;
        SafeFileHandle h = CreateFileW(path, 0x80, SHARE_ALL, IntPtr.Zero, OPEN_EXISTING, BACKUP_SEMANTICS, IntPtr.Zero);
        if (h.IsInvalid) { s.Error = "open " + Err(Marshal.GetLastWin32Error()); return s; }
        using (h)
        {
            IntPtr b = Marshal.AllocHGlobal(4096);
            IntPtr inb = Marshal.AllocHGlobal(4);
            try
            {
                if (GetFileInformationByHandleEx(h, 0, b, 40)) { s.LastWriteTime = Marshal.ReadInt64(b, 16); s.ChangeTime = Marshal.ReadInt64(b, 24); }
                else s.Error = "basicinfo " + Err(Marshal.GetLastWin32Error());
                if (GetFileInformationByHandleEx(h, 1, b, 24)) s.Size = Marshal.ReadInt64(b, 8);
                if (GetFileInformationByHandleEx(h, 18, b, 24))
                {
                    byte[] id = new byte[16]; Marshal.Copy(b + 8, id, 0, 16); s.FileId = BitConverter.ToString(id).Replace("-", "");
                }
                Marshal.WriteInt16(inb, 0, 2); Marshal.WriteInt16(inb, 2, 3);
                uint ret;
                if (DeviceIoControl(h, FSCTL_READ_FILE_USN_DATA, inb, 4, b, 4096, out ret, IntPtr.Zero))
                {
                    int major = Marshal.ReadInt16(b, 4);
                    if (major == 2) { s.Usn = Marshal.ReadInt64(b, 24); s.Reason = (uint)Marshal.ReadInt32(b, 40); }
                    else { s.Usn = Marshal.ReadInt64(b, 40); s.Reason = (uint)Marshal.ReadInt32(b, 56); }
                }
                else s.Error = (s.Error == null ? "" : s.Error + "; ") + "fileusn " + Err(Marshal.GetLastWin32Error());
            }
            finally { Marshal.FreeHGlobal(b); Marshal.FreeHGlobal(inb); }
        }
        return s;
    }
}
'@
Add-Type -TypeDefinition $cs

Write-Output "=== Windows: $([Environment]::OSVersion.VersionString); elevated: $(([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
Write-Output ""
Write-Output "=== 1. Journal access through different handles (query journal, then read journal)"
$GENERIC_READ = [uint32]2147483648
$root1 = "${Volume}:\"
foreach ($t in @(
        @("\\.\${Volume}:", [uint32]0, [uint32]0, 'volume device, access=0'),
        @("\\.\${Volume}:", [uint32]0x80, [uint32]0, 'volume device, FILE_READ_ATTRIBUTES'),
        @("\\.\${Volume}:", [uint32]0x100080, [uint32]0, 'volume device, FILE_READ_ATTRIBUTES|SYNCHRONIZE'),
        @("\\.\${Volume}:", [uint32]0x1, [uint32]0, 'volume device, FILE_READ_DATA'),
        @("\\.\${Volume}:", $GENERIC_READ, [uint32]0, 'volume device, GENERIC_READ'),
        @($root1, [uint32]0x80, [uint32]0x02000000, 'volume ROOT DIRECTORY handle, FILE_READ_ATTRIBUTES'),
        @($root1, [uint32]0x100001, [uint32]0x02000000, 'volume ROOT DIRECTORY handle, FILE_LIST_DIRECTORY|SYNCHRONIZE'),
        @($env:TEMP, [uint32]0x80, [uint32]0x02000000, 'ordinary directory (%TEMP%), FILE_READ_ATTRIBUTES'))) {
    [UsnProbe]::Matrix($t[0], $t[1], $t[2], $t[3])
}

Write-Output ""
Write-Output "=== 2. Per-file metadata with only FILE_READ_ATTRIBUTES (USN, ChangeTime, size, FileId)"
$root = Join-Path ([IO.Path]::GetTempPath()) ("tss-spike-s1-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $root, "$root\sub" | Out-Null
function Fmt($name, $s) {
    # FileId: first 8 bytes = NTFS record number (6 bytes) + sequence number (2 bytes, bumps when a record is reused)
    '{0,-22} usn={1,-11} change={2} write={3} size={4,-3} id={5}{6}' -f $name, $s.Usn,
        ([DateTime]::FromFileTimeUtc($s.ChangeTime).ToString('ss.fffffff')), ([DateTime]::FromFileTimeUtc($s.LastWriteTime).ToString('ss.fffffff')),
        $s.Size, $s.FileId.Substring(0, 16), $(if ($s.Error) { "  ERROR: $($s.Error)" } else { '' })
}
try {
    [IO.File]::WriteAllText("$root\a.txt", 'hello world 1'); [IO.File]::WriteAllText("$root\b.txt", 'bee'); [IO.File]::WriteAllText("$root\sub\c.txt", 'sea')
    Start-Sleep -Milliseconds 50
    function Show($label) {
        Write-Output "--- $label"
        foreach ($p in @("$root\a.txt", "$root\b.txt", "$root\b2.txt", "$root\sub\c.txt", "$root\sub\d.txt", "$root", "$root\sub")) {
            if (-not (Test-Path -LiteralPath $p)) { continue }
            $s = [UsnProbe]::Snap($p)
            Fmt (Split-Path $p -Leaf) $s
        }
    }
    Show 'A. baseline'
    $oldWrite = (Get-Item "$root\a.txt").LastWriteTimeUtc
    Start-Sleep -Milliseconds 300
    [IO.File]::WriteAllText("$root\a.txt", 'hello world 2'); (Get-Item "$root\a.txt").LastWriteTimeUtc = $oldWrite
    Show 'B. SAME-SIZE edit of a.txt, LastWriteTime restored to the old value (the "invisible" edit)'
    Start-Sleep -Milliseconds 300
    (Get-Item "$root\b.txt").LastWriteTimeUtc = (Get-Date).AddDays(-3).ToUniversalTime()
    Show 'C. b.txt touched (timestamp only, content unchanged)'
    Start-Sleep -Milliseconds 300
    Rename-Item "$root\b.txt" 'b2.txt'
    Show 'D. b.txt renamed to b2.txt'
    Start-Sleep -Milliseconds 300
    Remove-Item "$root\sub\c.txt"; [IO.File]::WriteAllText("$root\sub\d.txt", 'dee')
    Show 'E. sub\c.txt deleted, sub\d.txt added (watch the sub directory itself)'
    Start-Sleep -Milliseconds 300
    $before = [UsnProbe]::Snap("$root\sub"); [IO.File]::AppendAllText("$root\sub\d.txt", '!'); $after = [UsnProbe]::Snap("$root\sub")
    Write-Output "--- F. content edit of sub\d.txt: does the PARENT directory's USN/ChangeTime move?  parent usn $($before.Usn) -> $($after.Usn); parent change same: $($before.ChangeTime -eq $after.ChangeTime)"

    Write-Output ""
    Write-Output "=== 4. Write through a handle that stays OPEN: when do USN / ChangeTime / LastWriteTime move? (same-size in-place overwrite)"
    $p = "$root\open.txt"; [IO.File]::WriteAllText($p, 'v1v1'); Start-Sleep -Milliseconds 300
    $fs = New-Object IO.FileStream($p, [IO.FileMode]::Open, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
    Fmt 'open: before write' ([UsnProbe]::Snap($p))
    $bytes = [Text.Encoding]::ASCII.GetBytes('v2v2'); $fs.Write($bytes, 0, 4); $fs.Flush()
    Fmt 'open: after write+flush' ([UsnProbe]::Snap($p))
    Start-Sleep -Milliseconds 1500
    Fmt 'open: +1.5 s, still open' ([UsnProbe]::Snap($p))
    $fs.Dispose()
    Fmt 'after handle closed' ([UsnProbe]::Snap($p))
    Write-Output ("content now: " + [IO.File]::ReadAllText($p))

    Write-Output ""
    Write-Output "=== 5. Replace patterns (all same size, LastWriteTime forced equal to the old one): are they all detectable?"
    $t = "$root\target.txt"; [IO.File]::WriteAllText($t, 'AAAA'); Start-Sleep -Milliseconds 300
    $mt = (Get-Item $t).LastWriteTimeUtc
    Fmt 'original' ([UsnProbe]::Snap($t))
    [IO.File]::WriteAllText("$root\tmp1.txt", 'BBBB'); (Get-Item "$root\tmp1.txt").LastWriteTimeUtc = $mt
    Start-Sleep -Milliseconds 300; Move-Item -LiteralPath "$root\tmp1.txt" -Destination $t -Force
    Fmt 'after Move-Item -Force' ([UsnProbe]::Snap($t))
    [IO.File]::WriteAllText("$root\tmp2.txt", 'CCCC'); (Get-Item "$root\tmp2.txt").LastWriteTimeUtc = $mt
    Start-Sleep -Milliseconds 300; [IO.File]::Replace("$root\tmp2.txt", $t, [NullString]::Value)
    Fmt 'after File.Replace' ([UsnProbe]::Snap($t))
    [IO.File]::WriteAllText("$root\src.txt", 'DDDD'); (Get-Item "$root\src.txt").LastWriteTimeUtc = $mt
    Start-Sleep -Milliseconds 300; Copy-Item -LiteralPath "$root\src.txt" -Destination $t -Force
    Fmt 'after Copy-Item -Force' ([UsnProbe]::Snap($t))
    Write-Output ("(final content of target.txt: " + [IO.File]::ReadAllText($t) + ")")
}
finally { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }

Write-Output ""
Write-Output "=== 6. Does the per-file USN survive the journal being trimmed? (sample of OLD files, attributes only)"
$j = [UsnProbe]::JournalInfo($Volume)
'journal: id=0x{0:x} firstUsn={1} nextUsn={2} maxSize={3:N0} bytes (live window = {4:N1} MB)' -f $j[0], $j[1], $j[2], $j[3], (($j[2] - $j[1]) / 1MB)
$sample = @(Get-ChildItem -LiteralPath 'C:\Windows\System32' -File -ErrorAction SilentlyContinue | Where-Object { $_.LastWriteTime -lt (Get-Date).AddDays(-90) } | Get-Random -Count 400)
$zero = 0; $below = 0; $inside = 0; $err = 0; $minUsn = [long]::MaxValue
foreach ($f in $sample) {
    $s = [UsnProbe]::Snap($f.FullName)
    if ($s.Usn -lt 0) { $err++ } elseif ($s.Usn -eq 0) { $zero++ } elseif ($s.Usn -lt $j[1]) { $below++; if ($s.Usn -lt $minUsn) { $minUsn = $s.Usn } } else { $inside++ }
}
"sampled {0} old System32 files: usn=0: {1}; usn < firstUsn (record trimmed from the journal, USN still returned): {2}; usn inside live window: {3}; errors: {4}" -f $sample.Count, $zero, $below, $inside, $err
if ($below -gt 0) { "smallest trimmed-away USN seen: $minUsn  (journal started at $($j[1]))" }

Write-Output ""
Write-Output "=== 7. Journal churn (journal read is impossible without admin; this only shows how fast a reader would have lost history)"
$n1 = [UsnProbe]::NextUsn($Volume); Start-Sleep -Seconds 20; $n2 = [UsnProbe]::NextUsn($Volume)
'idle: {0:N0} bytes in 20 s = {1:N2} MB/min' -f ($n2 - $n1), (($n2 - $n1) * 3 / 1MB)
$burst = Join-Path ([IO.Path]::GetTempPath()) ("tss-spike-burst-" + [Guid]::NewGuid().ToString('N').Substring(0, 8)); New-Item -ItemType Directory $burst | Out-Null
$b1 = [UsnProbe]::NextUsn($Volume)
1..500 | ForEach-Object { [IO.File]::WriteAllText("$burst\f$_.txt", 'x') }
Remove-Item -LiteralPath $burst -Recurse -Force
$b2 = [UsnProbe]::NextUsn($Volume)
'burst: create+delete 500 tiny files = {0:N0} bytes of journal ({1:N0} B per file) -> a git clone of 100k files is roughly {2:N0} MB, vs a 32 MB default window' -f ($b2 - $b1), (($b2 - $b1) / 500), ((($b2 - $b1) / 500) * 100000 / 1MB)

Write-Output ""
Write-Output "=== 8. Size of the REAL skills folder (metadata only, nothing is read or changed)"
$skills = Join-Path $env:USERPROFILE '.copilot\skills'
if (Test-Path $skills) {
    $all = @(Get-ChildItem -LiteralPath $skills -Recurse -Force -File -ErrorAction SilentlyContinue)
    $dirs = @(Get-ChildItem -LiteralPath $skills -Recurse -Force -Directory -ErrorAction SilentlyContinue)
    $sum = ($all | Measure-Object -Property Length -Sum).Sum
    $max = ($all | Measure-Object -Property Length -Maximum).Maximum
    'skills folder: {0} files, {1} directories, {2:N0} bytes total, largest file {3:N0} bytes' -f $all.Count, $dirs.Count, $sum, $max
}
