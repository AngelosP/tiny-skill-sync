//! Spike S1b: cost and behaviour of NTFS change tracking, measured in Rust.
//!
//! ```text
//! cargo run --release --example s1b_fingerprint -- corpus small|medium|large
//! cargo run --release --example s1b_fingerprint -- bench  small|medium|large [reps]
//! cargo run --release --example s1b_fingerprint -- race   [iterations-per-delay]
//! cargo run --release --example s1b_fingerprint -- tick
//! cargo run --release --example s1b_fingerprint -- openwriter
//! cargo run --release --example s1b_fingerprint -- mmap
//! cargo run --release --example s1b_fingerprint -- overflow
//! cargo run --release --example s1b_fingerprint -- cleanup
//! ```
//!
//! Everything lives under `%TEMP%\tss-s1b`. Nothing outside that folder is touched.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_ALG_HANDLE, BCRYPT_HASH_REUSABLE_FLAG, BCRYPT_SHA256_ALGORITHM, BCryptHash, BCryptOpenAlgorithmProvider,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN, FILE_GENERIC_READ, FILE_ID_EXTD_DIR_INFO,
    FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo, FileStandardInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING, ReadDirectoryChangesW, ReadFile,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::FSCTL_READ_FILE_USN_DATA;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, FlushViewOfFile, MEM_COMMIT, MEM_RESERVE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, WaitForSingleObject};

#[link(name = "ntdll")]
unsafe extern "system" {
    /// All values are in 100 ns units. `coarsest` is the largest period, `finest` the smallest.
    fn NtQueryTimerResolution(coarsest: *mut u32, finest: *mut u32, current: *mut u32) -> i32;
}

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn root() -> PathBuf {
    std::env::temp_dir().join("tss-s1b")
}

fn wide_nul(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open_raw(path_nul: &[u16], access: u32, flags: u32) -> Option<Handle> {
    let h = unsafe { CreateFileW(path_nul.as_ptr(), access, SHARE_ALL, null(), OPEN_EXISTING, flags, null_mut()) };
    if h == INVALID_HANDLE_VALUE { None } else { Some(Handle(h)) }
}

fn spin(d: Duration) {
    let t = Instant::now();
    while t.elapsed() < d {
        std::hint::spin_loop();
    }
}

// ---------------------------------------------------------------- corpus

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn size(&mut self) -> usize {
        let r = self.next() % 100;
        let x = self.next() as usize;
        match r {
            0..=59 => 200 + x % 3000,
            60..=89 => 3000 + x % 9000,
            _ => 12000 + x % 26000,
        }
    }
}

fn write_file(path: &Path, rng: &mut Rng) -> u64 {
    let n = rng.size();
    let mut data = vec![0u8; n];
    for chunk in data.chunks_mut(8) {
        let v = rng.next().to_le_bytes();
        let l = chunk.len();
        chunk.copy_from_slice(&v[..l]);
    }
    fs::write(path, &data).unwrap();
    n as u64
}

fn cmd_corpus(name: &str) {
    type F = Box<dyn Fn(usize) -> usize>;
    let (skills, extras, subs): (usize, F, F) = match name {
        // Mirrors the real skills folder: 17 skills, 84 files, 29 directories.
        "small" => (17, Box::new(|i| if i == 16 { 3 } else { 4 }), Box::new(|i| usize::from(i < 12))),
        "medium" => (100, Box::new(|_| 19), Box::new(|_| 4)),
        "large" => (500, Box::new(|_| 39), Box::new(|_| 5)),
        _ => panic!("unknown corpus {name}"),
    };
    let dir = root().join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    let t = Instant::now();
    for s in 0..skills {
        let sd = dir.join(format!("skill-{s:03}"));
        fs::create_dir_all(&sd).unwrap();
        dirs += 1;
        bytes += write_file(&sd.join("SKILL.md"), &mut rng);
        files += 1;
        let nsub = subs(s);
        let mut places = vec![sd.clone()];
        for k in 0..nsub {
            let d = sd.join(format!("sub{k}"));
            fs::create_dir_all(&d).unwrap();
            dirs += 1;
            places.push(d);
        }
        for e in 0..extras(s) {
            let d = if nsub == 0 { &places[0] } else { &places[1 + e % nsub] };
            bytes += write_file(&d.join(format!("file-{e:03}.dat")), &mut rng);
            files += 1;
        }
    }
    println!("corpus {name}: {files} files, {dirs} directories, {bytes} bytes, created in {:.1} s at {}", t.elapsed().as_secs_f64(), dir.display());
}

// ---------------------------------------------------------------- enumeration and fingerprints

#[derive(Clone)]
struct Entry {
    path: Vec<u16>, // NUL terminated
    size: u64,
    last_write: i64,
    change: i64,
    file_id: [u8; 16],
}

/// Bulk enumeration: one directory handle per directory, no per-file open.
fn enumerate(root: &Path) -> Vec<Entry> {
    let mut files = Vec::new();
    let mut stack: Vec<Vec<u16>> = vec![root.as_os_str().encode_wide().collect()];
    let mut buf = vec![0u64; 8192]; // 64 KiB, 8-byte aligned
    while let Some(dir) = stack.pop() {
        let mut p = dir.clone();
        p.push(0);
        let Some(h) = open_raw(&p, FILE_LIST_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS) else { continue };
        let mut class = FileIdExtdDirectoryRestartInfo;
        loop {
            let ok = unsafe { GetFileInformationByHandleEx(h.0, class, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32) };
            if ok == 0 {
                break;
            }
            class = FileIdExtdDirectoryInfo;
            let base = buf.as_ptr().cast::<u8>();
            let mut off = 0usize;
            loop {
                let rec = unsafe { &*base.add(off).cast::<FILE_ID_EXTD_DIR_INFO>() };
                let name = unsafe { std::slice::from_raw_parts(rec.FileName.as_ptr(), rec.FileNameLength as usize / 2) };
                let dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
                if !dot {
                    let mut full = dir.clone();
                    full.push(b'\\' as u16);
                    full.extend_from_slice(name);
                    if rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        if rec.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                            stack.push(full);
                        }
                    } else {
                        full.push(0);
                        files.push(Entry {
                            path: full,
                            size: rec.EndOfFile as u64,
                            last_write: rec.LastWriteTime,
                            change: rec.ChangeTime,
                            file_id: rec.FileId.Identifier,
                        });
                    }
                }
                if rec.NextEntryOffset == 0 {
                    break;
                }
                off += rec.NextEntryOffset as usize;
            }
        }
    }
    files
}

fn usn_of(h: &Handle, out: &mut [u8; 1024]) -> Option<i64> {
    let mut ret = 0u32;
    let ok = unsafe { DeviceIoControl(h.0, FSCTL_READ_FILE_USN_DATA, null(), 0, out.as_mut_ptr().cast(), 1024, &mut ret, null_mut()) };
    if ok == 0 || ret < 48 {
        return None;
    }
    let major = u16::from_le_bytes([out[4], out[5]]);
    let off = match major {
        2 => 24,
        3 => 40,
        _ => return None,
    };
    Some(i64::from_le_bytes(out[off..off + 8].try_into().unwrap()))
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Fp {
    size: i64,
    last_write: i64,
    change: i64,
    usn: i64,
    file_id: [u8; 16],
}

/// Attribute-only handle: three info calls and the file USN.
fn capture(path_nul: &[u16]) -> Option<Fp> {
    let h = open_raw(path_nul, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)?;
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let mut std_info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(h.0, FileBasicInfo, (&mut basic as *mut FILE_BASIC_INFO).cast(), size_of::<FILE_BASIC_INFO>() as u32)
            & GetFileInformationByHandleEx(h.0, FileStandardInfo, (&mut std_info as *mut FILE_STANDARD_INFO).cast(), size_of::<FILE_STANDARD_INFO>() as u32)
            & GetFileInformationByHandleEx(h.0, FileIdInfo, (&mut id as *mut FILE_ID_INFO).cast(), size_of::<FILE_ID_INFO>() as u32)
    };
    if ok == 0 {
        return None;
    }
    let mut out = [0u8; 1024];
    let usn = usn_of(&h, &mut out)?;
    Some(Fp { size: std_info.EndOfFile, last_write: basic.LastWriteTime, change: basic.ChangeTime, usn, file_id: id.FileId.Identifier })
}

fn open_sha256() -> BCRYPT_ALG_HANDLE {
    let mut alg: BCRYPT_ALG_HANDLE = null_mut();
    let st = unsafe { BCryptOpenAlgorithmProvider(&mut alg, BCRYPT_SHA256_ALGORITHM, null(), BCRYPT_HASH_REUSABLE_FLAG) };
    assert!(st >= 0, "BCryptOpenAlgorithmProvider failed: {st:#x}");
    alg
}

fn sha256_file(path_nul: &[u16], size: u64, buf: &mut Vec<u8>, alg: BCRYPT_ALG_HANDLE) -> Option<[u8; 32]> {
    let h = open_raw(path_nul, FILE_GENERIC_READ, FILE_FLAG_SEQUENTIAL_SCAN)?;
    let want = size as usize;
    if buf.len() < want.max(1) {
        buf.resize(want.max(1), 0);
    }
    let mut got = 0u32;
    if want > 0 && unsafe { ReadFile(h.0, buf.as_mut_ptr().cast(), want as u32, &mut got, null_mut()) } == 0 {
        return None;
    }
    let mut out = [0u8; 32];
    let st = unsafe { BCryptHash(alg, null_mut(), 0, buf.as_mut_ptr(), got, out.as_mut_ptr(), 32) };
    (st >= 0).then_some(out)
}

/// Content hash with the file cache bypassed: a proxy for cold data. Metadata stays warm.
fn sha256_file_uncached(path_nul: &[u16], size: u64, aligned: *mut u8, cap: usize, alg: BCRYPT_ALG_HANDLE) -> Option<[u8; 32]> {
    let h = open_raw(path_nul, FILE_GENERIC_READ, FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)?;
    let want = (size as usize + 4095) & !4095;
    if want == 0 || want > cap {
        return None;
    }
    let mut got = 0u32;
    if unsafe { ReadFile(h.0, aligned.cast(), want as u32, &mut got, null_mut()) } == 0 {
        return None;
    }
    let mut out = [0u8; 32];
    let st = unsafe { BCryptHash(alg, null_mut(), 0, aligned, got, out.as_mut_ptr(), 32) };
    (st >= 0).then_some(out)
}

// ---------------------------------------------------------------- bench

struct Stats {
    min: Duration,
    med: Duration,
    p95: Duration,
}

fn time(reps: usize, mut f: impl FnMut()) -> Stats {
    let mut v = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        f();
        v.push(t.elapsed());
    }
    v.sort();
    Stats { min: v[0], med: v[v.len() / 2], p95: v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)] }
}

fn walk_std(dir: &Path, files: &mut u64, bytes: &mut u64, mt: &mut u64) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let md = e.metadata().unwrap();
        if md.is_dir() {
            walk_std(&e.path(), files, bytes, mt);
        } else {
            *files += 1;
            *bytes += md.len();
            *mt ^= md.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
        }
    }
}

fn mem() -> (usize, usize) {
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast(), c.cb) };
    (c.PeakWorkingSetSize, c.PrivateUsage)
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

fn cmd_bench(name: &str, reps: usize) {
    let dir = root().join(name);
    let entries = enumerate(&dir);
    let total_bytes: u64 = entries.iter().map(|e| e.size).sum();
    let n = entries.len();
    println!("corpus {name}: {n} files, {total_bytes} bytes, {reps} reps, warm cache");
    let alg = open_sha256();
    let mut buf = vec![0u8; 65536];
    let mut usn_buf = [0u8; 1024];

    // One untimed pass of everything to warm caches and lazy initialisation.
    for e in &entries {
        black_box(capture(&e.path));
        black_box(sha256_file(&e.path, e.size, &mut buf, alg));
    }

    let m1 = time(reps, || {
        let (mut f, mut b, mut m) = (0, 0, 0);
        walk_std(&dir, &mut f, &mut b, &mut m);
        black_box((f, b, m));
    });
    let m2 = time(reps, || {
        black_box(enumerate(&dir));
    });
    let m3 = time(reps, || {
        let es = enumerate(&dir);
        let mut acc = 0i64;
        for e in &es {
            if let Some(h) = open_raw(&e.path, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT) {
                acc ^= usn_of(&h, &mut usn_buf).unwrap_or(0);
            }
        }
        black_box(acc);
    });
    let m3b = time(reps, || {
        let es = enumerate(&dir);
        let mut acc = 0i64;
        for e in &es {
            acc ^= capture(&e.path).map_or(0, |f| f.usn);
        }
        black_box(acc);
    });
    let m4 = time(reps, || {
        let es = enumerate(&dir);
        let mut acc = 0u8;
        for e in &es {
            acc ^= sha256_file(&e.path, e.size, &mut buf, alg).map_or(0, |d| d[0]);
        }
        black_box(acc);
    });
    const ALIGNED_CAP: usize = 1 << 20;
    let aligned = unsafe { VirtualAlloc(null(), ALIGNED_CAP, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }.cast::<u8>();
    assert!(!aligned.is_null());
    let m4c = time(reps, || {
        let es = enumerate(&dir);
        let mut acc = 0u8;
        for e in &es {
            acc ^= sha256_file_uncached(&e.path, e.size, aligned, ALIGNED_CAP, alg).map_or(0, |d| d[0]);
        }
        black_box(acc);
    });

    // Verification as the app would do it: compare the enumeration with a stored fingerprint list.
    let stored: Vec<(Vec<u16>, Entry)> = entries.iter().map(|e| (e.path.clone(), e.clone())).collect();
    let m5 = time(reps, || {
        let es = enumerate(&dir);
        let mut mismatches = 0usize;
        for (e, (_, s)) in es.iter().zip(stored.iter()) {
            if e.size != s.size || e.last_write != s.last_write || e.change != s.change || e.file_id != s.file_id {
                mismatches += 1;
            }
        }
        black_box(mismatches);
    });

    println!("| method | min ms | median ms | p95 ms | median us/file |");
    println!("| --- | --- | --- | --- | --- |");
    let row = |label: &str, s: &Stats| {
        println!("| {label} | {} | {} | {} | {:.1} |", ms(s.min), ms(s.med), ms(s.p95), s.med.as_secs_f64() * 1e6 / n as f64);
    };
    row("M1 std::fs walk (size + mtime only)", &m1);
    row("M2 bulk enumeration (FileId, size, mtime, ChangeTime)", &m2);
    row("M5 M2 + compare against stored list (no USN)", &m5);
    row("M3 M2 + per-file attribute open + USN", &m3);
    row("M3b M2 + per-file attribute open + 3 info calls + USN", &m3b);
    row("M4 M2 + open, read, SHA-256 of every file", &m4);
    row("M4c M2 + open, read with file cache bypassed, SHA-256 (cold-data proxy)", &m4c);
    let (peak, private) = mem();
    println!("process peak working set {:.1} MiB, private bytes {:.1} MiB", peak as f64 / 1048576.0, private as f64 / 1048576.0);
}

// ---------------------------------------------------------------- race

/// The coarse system time is what NTFS stamps into ChangeTime and LastWriteTime.
fn coarse_now() -> i64 {
    let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

/// Reports the timer resolution and how the coarse system time actually advances (spin-sampled).
fn print_tick(label: &str, sample: Duration) {
    let (mut coarsest, mut finest, mut current) = (0u32, 0u32, 0u32);
    unsafe { NtQueryTimerResolution(&mut coarsest, &mut finest, &mut current) };
    let mut steps: Vec<i64> = Vec::new();
    let mut last = coarse_now();
    let end = Instant::now() + sample;
    while Instant::now() < end {
        let now = coarse_now();
        if now != last {
            steps.push(now - last);
            last = now;
        }
    }
    steps.sort();
    let q = |f: f64| steps.get(((steps.len() as f64 * f) as usize).min(steps.len().saturating_sub(1))).map_or(0.0, |&v| v as f64 / 10_000.0);
    println!(
        "[{label}] timer resolution: current {:.3} ms (finest {:.3} ms, coarsest {:.3} ms). Coarse system time advanced {} times in {} ms; step min {:.3} / median {:.3} / p95 {:.3} / max {:.3} ms",
        current as f64 / 10_000.0,
        finest as f64 / 10_000.0,
        coarsest as f64 / 10_000.0,
        steps.len(),
        sample.as_millis(),
        q(0.0),
        q(0.5),
        q(0.95),
        q(1.0)
    );
}

fn cmd_tick() {
    print_tick("now", Duration::from_secs(2));
}

fn cmd_race(iters: usize) {
    print_tick("before", Duration::from_millis(500));
    let dir = root().join("race");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("x.bin");
    fs::write(&p, [0u8; 64]).unwrap();
    let pw = wide_nul(&p);
    println!("Same-size in-place edit after the app's own write + fingerprint capture. {iters} iterations per delay.");
    println!("| delay between capture and edit | ChangeTime equal | LastWriteTime equal | USN equal | ChangeTime AND USN equal |");
    println!("| --- | --- | --- | --- | --- |");
    let mut prev: Option<i64> = None;
    let mut min_positive_delta = i64::MAX;
    let mut distinct = std::collections::BTreeSet::new();
    for delay_us in [0u64, 500, 1000, 2000, 4000, 8000, 16000, 32000] {
        let (mut same_c, mut same_w, mut same_u, mut same_cu) = (0, 0, 0, 0);
        for i in 0..iters {
            fs::write(&p, vec![(i % 251) as u8; 64]).unwrap();
            let f1 = capture(&pw).unwrap();
            spin(Duration::from_micros(delay_us));
            {
                let mut f = OpenOptions::new().write(true).open(&p).unwrap();
                f.write_all(&vec![((i + 100) % 251) as u8; 64]).unwrap();
            }
            let f2 = capture(&pw).unwrap();
            if f1.change == f2.change {
                same_c += 1;
            }
            if f1.last_write == f2.last_write {
                same_w += 1;
            }
            if f1.usn == f2.usn {
                same_u += 1;
            }
            if f1.change == f2.change && f1.usn == f2.usn {
                same_cu += 1;
            }
            distinct.insert(f2.change);
            if let Some(pv) = prev {
                let d = f2.change - pv;
                if d > 0 {
                    min_positive_delta = min_positive_delta.min(d);
                }
            }
            prev = Some(f2.change);
        }
        println!("| {:>5} us | {same_c} | {same_w} | {same_u} | {same_cu} |", delay_us);
    }
    println!(
        "distinct ChangeTime values seen: {}; smallest positive step between consecutive values: {:.3} ms",
        distinct.len(),
        min_positive_delta as f64 / 10_000.0
    );
    print_tick("after", Duration::from_millis(500));
}

// ---------------------------------------------------------------- open writer

fn moved(a: i64, b: i64) -> &'static str {
    if a != b { "moved" } else { "same" }
}

/// A writer keeps its handle open. Does bulk enumeration see the change, and does the per-file query?
fn cmd_openwriter() {
    let dir = root().join("openwriter");
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("w.bin");
    fs::write(&p, vec![1u8; 64]).unwrap();
    let pw = wide_nul(&p);
    std::thread::sleep(Duration::from_millis(100));
    let be = enumerate(&dir).remove(0);
    let bf = capture(&pw).unwrap();
    println!("Writer opens the file (shared), writes, and keeps the handle open. Each row compares with the baseline taken before the open.");
    println!("| stage | enum size | enum LastWrite | enum ChangeTime | handle size | handle LastWrite | handle ChangeTime | handle USN |");
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    let show = |label: &str| {
        let e = enumerate(&dir).remove(0);
        let f = capture(&pw).unwrap();
        println!(
            "| {label} | {} | {} | {} | {} | {} | {} | {} |",
            moved(be.size as i64, e.size as i64),
            moved(be.last_write, e.last_write),
            moved(be.change, e.change),
            moved(bf.size, f.size),
            moved(bf.last_write, f.last_write),
            moved(bf.change, f.change),
            moved(bf.usn, f.usn)
        );
    };
    let mut w = OpenOptions::new().write(true).open(&p).unwrap();
    show("handle opened, nothing written");
    w.write_all(&[2u8; 64]).unwrap();
    show("same-size overwrite, handle still open");
    std::thread::sleep(Duration::from_secs(1));
    show("+1 s idle");
    std::thread::sleep(Duration::from_secs(4));
    show("+5 s idle");
    w.write_all(&[3u8; 4096]).unwrap();
    show("append 4096 B, handle still open");
    std::thread::sleep(Duration::from_secs(5));
    show("+5 s idle after append");
    w.sync_all().unwrap();
    show("after FlushFileBuffers");
    drop(w);
    show("after closing the handle");
    println!("(a normal read sees the final size: {})", fs::metadata(&p).unwrap().len());
}

// ---------------------------------------------------------------- mmap

fn show(label: &str, base: &Fp, now: &Fp) {
    println!(
        "| {label} | {} | {} | {} |",
        if now.change != base.change { "moved" } else { "same" },
        if now.last_write != base.last_write { "moved" } else { "same" },
        if now.usn != base.usn { "moved" } else { "same" },
    );
}

fn cmd_mmap() {
    let dir = root().join("mmap");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("m.bin");
    fs::write(&p, vec![0u8; 65536]).unwrap();
    let pw = wide_nul(&p);
    let fp0 = capture(&pw).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        let h = CreateFileW(pw.as_ptr(), GENERIC_READ | GENERIC_WRITE, SHARE_ALL, null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, null_mut());
        assert!(h != INVALID_HANDLE_VALUE);
        let m = CreateFileMappingW(h, null(), PAGE_READWRITE, 0, 0, null());
        assert!(!m.is_null());
        let v = MapViewOfFile(m, FILE_MAP_WRITE, 0, 0, 0);
        assert!(!v.Value.is_null());
        let ptr = v.Value.cast::<u8>();
        println!("Baseline fingerprint taken before mapping. Each row compares with that baseline.");
        println!("| stage | ChangeTime | LastWriteTime | USN |");
        println!("| --- | --- | --- | --- |");
        show("view mapped, nothing written", &fp0, &capture(&pw).unwrap());
        ptr.add(100).write_volatile(0xAB);
        let visible = fs::read(&p).unwrap()[100];
        println!("(byte written through the view is visible to a normal read at once: {})", visible == 0xAB);
        show("byte written through the view", &fp0, &capture(&pw).unwrap());
        for s in 1..=4 {
            std::thread::sleep(Duration::from_secs(1));
            show(&format!("view still mapped, +{s} s idle"), &fp0, &capture(&pw).unwrap());
        }
        FlushViewOfFile(ptr.cast(), 0);
        show("after FlushViewOfFile", &fp0, &capture(&pw).unwrap());
        UnmapViewOfFile(v);
        CloseHandle(m);
        show("after UnmapViewOfFile", &fp0, &capture(&pw).unwrap());
        CloseHandle(h);
        show("after closing the file handle", &fp0, &capture(&pw).unwrap());
        std::thread::sleep(Duration::from_secs(2));
        show("+2 s after closing", &fp0, &capture(&pw).unwrap());
    }
}

// ---------------------------------------------------------------- ReadDirectoryChangesW overflow

fn count_records(buf: &[u32], bytes: u32) -> usize {
    let base = buf.as_ptr().cast::<u8>();
    let (mut off, mut n) = (0usize, 0usize);
    while (off as u32) < bytes {
        let rec = unsafe { &*base.add(off).cast::<FILE_NOTIFY_INFORMATION>() };
        n += 1;
        if rec.NextEntryOffset == 0 {
            break;
        }
        off += rec.NextEntryOffset as usize;
    }
    n
}

fn cmd_overflow() {
    let dir = root().join("watch");
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    let dw = wide_nul(&dir);
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION;
    println!("A watcher is armed, consumes one event, and is NOT re-armed while N small files are created. Then it is re-armed.");
    println!("| buffer bytes | files created while not listening | GetOverlappedResult | bytes | last error | records |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for bufsize in [1024u32, 4096, 65536] {
        for n in [5usize, 10, 20, 40, 80, 160, 320, 640, 1280, 2560] {
            unsafe {
                let h = CreateFileW(
                    dw.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    SHARE_ALL,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    null_mut(),
                );
                assert!(h != INVALID_HANDLE_VALUE);
                let _guard = Handle(h);
                let ev = CreateEventW(null(), 1, 0, null());
                let _evg = Handle(ev);
                let mut buf = vec![0u32; bufsize as usize / 4];
                let mut ov: OVERLAPPED = std::mem::zeroed();
                ov.hEvent = ev;
                // Arm and consume the first event.
                ReadDirectoryChangesW(h, buf.as_mut_ptr().cast(), bufsize, 1, filter, null_mut(), &mut ov, None);
                fs::write(dir.join("trigger.tmp"), b"x").unwrap();
                WaitForSingleObject(ev, 2000);
                let mut got = 0u32;
                GetOverlappedResult(h, &ov, &mut got, 0);
                // Not listening now.
                for i in 0..n {
                    fs::write(dir.join(format!("f{i:05}.tmp")), b"x").unwrap();
                }
                // Re-arm and read what the kernel kept.
                let mut ov2: OVERLAPPED = std::mem::zeroed();
                ov2.hEvent = ev;
                std::ptr::write_bytes(buf.as_mut_ptr(), 0, buf.len());
                ReadDirectoryChangesW(h, buf.as_mut_ptr().cast(), bufsize, 1, filter, null_mut(), &mut ov2, None);
                let w = WaitForSingleObject(ev, 2000);
                let mut got2 = 0u32;
                let ok = GetOverlappedResult(h, &ov2, &mut got2, 0);
                let err = if ok == 0 { GetLastError() } else { 0 };
                println!(
                    "| {bufsize} | {n} | {} | {got2} | {err} | {} |",
                    if w != 0 {
                        "no completion"
                    } else if ok != 0 {
                        "success"
                    } else {
                        "failed"
                    },
                    if ok != 0 { count_records(&buf, got2) } else { 0 }
                );
                for i in 0..n {
                    fs::remove_file(dir.join(format!("f{i:05}.tmp"))).ok();
                }
                fs::remove_file(dir.join("trigger.tmp")).ok();
            }
        }
    }
    println!(
        "(An overflow is reported as a successful completion with 0 bytes, which is STATUS_NOTIFY_ENUM_DIR. GetOverlappedResult returned success and last error 0; error 1022 is not surfaced this way.)"
    );
}

fn cmd_cleanup() {
    match fs::remove_dir_all(root()) {
        Ok(()) => println!("removed {}", root().display()),
        Err(e) => println!("nothing removed ({e})"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("corpus") => cmd_corpus(args.get(1).map_or("small", String::as_str)),
        Some("bench") => cmd_bench(args.get(1).map_or("small", String::as_str), args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30)),
        Some("race") => cmd_race(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(300)),
        Some("tick") => cmd_tick(),
        Some("openwriter") => cmd_openwriter(),
        Some("mmap") => cmd_mmap(),
        Some("overflow") => cmd_overflow(),
        Some("cleanup") => cmd_cleanup(),
        _ => eprintln!("usage: s1b_fingerprint corpus|bench|race|tick|openwriter|mmap|overflow|cleanup ..."),
    }
}
