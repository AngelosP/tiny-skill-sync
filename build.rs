//! Embeds `app.manifest` (visual styles, per-monitor DPI, UTF-8, long-path opt-in, no elevation), a version resource
//! (the name and version Windows shows for the program, as in Task Manager's startup apps) and the icon
//! (`assets\tiny-skill-sync.ico`, which Explorer, a taskbar pin and Settings show) into the executables with the MSVC
//! linker. No resource compiler or extra crate is needed: the resources are written here as a `.res` file, which the
//! linker takes as it is.

use std::path::Path;

/// The name of the product, as `board::APP_NAME`.
const PRODUCT: &str = "Tiny Skill Sync";
/// Each executable, and the name Windows shows for it.
const PROGRAMS: [(&str, &str); 2] = [("tiny-skill-sync", PRODUCT), ("tss", "Tiny Skill Sync command line")];
/// Written by `scripts\dev.ps1 logo` from `src\logo.rs`; a test keeps it the same as what the app draws.
const ICON: &str = "assets/tiny-skill-sync.ico";
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
/// US English, in Unicode: the language of the strings.
const LANGUAGE: u16 = 0x0409;
const CODE_PAGE: u16 = 1200;

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let dir = env("CARGO_MANIFEST_DIR");
    for kind in ["bins", "examples"] {
        println!("cargo:rustc-link-arg-{kind}=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-{kind}=/MANIFESTUAC:NO");
        println!("cargo:rustc-link-arg-{kind}=/MANIFESTINPUT:{dir}\\app.manifest");
    }
    let icon = icon_res(&std::fs::read(Path::new(&dir).join(ICON)).expect("assets\\tiny-skill-sync.ico can be read"));
    let out = env("OUT_DIR");
    for (name, description) in PROGRAMS {
        let res = Path::new(&out).join(format!("{name}.res"));
        let mut data = version_res(name, description);
        data.extend(&icon);
        std::fs::write(&res, data).expect("the resources can be written");
        println!("cargo:rustc-link-arg-bin={name}={}", res.display());
        println!("cargo:rustc-link-arg-bin={name}=/SWAPRUN:NET");
    }
}

/// The resources of an icon file: each image as an `RT_ICON` numbered from 1, then the group (`RT_GROUP_ICON` 1) that
/// lists them, which is the ICO file's directory with the image numbers in place of the file offsets.
fn icon_res(ico: &[u8]) -> Vec<u8> {
    let word = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let dword = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]);
    assert!(ico.len() >= 6 && word(0) == 0 && word(2) == 1, "{ICON} is an icon file");
    let count = word(4);
    let mut res = Vec::new();
    let mut group = Vec::new();
    for word in [0, 1, count] {
        group.extend(word.to_le_bytes());
    }
    for i in 0..count {
        let at = 6 + 16 * i as usize;
        let (len, offset) = (dword(at + 8) as usize, dword(at + 12) as usize);
        let image = ico.get(offset..offset + len).expect("an image inside the icon file");
        // Moveable and discardable, as resource compilers write icons.
        res.extend(entry(RT_ICON, i + 1, 0x1010, LANGUAGE, image));
        // Width, height, colours, reserved, planes, bits and size are as in the file; then the image's number.
        group.extend(&ico[at..at + 12]);
        group.extend((i + 1).to_le_bytes());
    }
    res.extend(entry(RT_GROUP_ICON, 1, 0x1030, LANGUAGE, &group));
    res
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("cargo sets {name}"))
}

/// A `.res` file with the version resource of a program.
fn version_res(name: &str, description: &str) -> Vec<u8> {
    let version = env("CARGO_PKG_VERSION");
    let number = |part: &str| env(&format!("CARGO_PKG_VERSION_{part}")).parse::<u32>().expect("a version number fits 16 bits") & 0xFFFF;
    let high = (number("MAJOR") << 16) | number("MINOR");
    let low = number("PATCH") << 16;
    // VS_FIXEDFILEINFO: signature, structure version, file and product versions, flags mask, flags, Windows NT, an
    // application, no subtype, no date.
    let fixed: Vec<u8> = [0xFEEF_04BD, 0x0001_0000, high, low, high, low, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0u32].iter().flat_map(|v| v.to_le_bytes()).collect();
    let file = format!("{name}.exe");
    let strings: Vec<Vec<u8>> = [
        ("FileDescription", description),
        ("FileVersion", &version),
        ("InternalName", name),
        ("OriginalFilename", &file),
        ("ProductName", PRODUCT),
        ("ProductVersion", &version),
    ]
    .iter()
    .map(|(key, text)| block(key, Value::Text(text), &[]))
    .collect();
    let table = block(&format!("{LANGUAGE:04x}{CODE_PAGE:04x}"), Value::None, &strings);
    let translation: Vec<u8> = [LANGUAGE, CODE_PAGE].iter().flat_map(|v| v.to_le_bytes()).collect();
    let info = block(
        "VS_VERSION_INFO",
        Value::Binary(&fixed),
        &[block("StringFileInfo", Value::None, &[table]), block("VarFileInfo", Value::None, &[block("Translation", Value::Binary(&translation), &[])])],
    );
    // Every 32-bit `.res` file starts with an empty entry.
    let mut res = entry(0, 0, 0, 0, &[]);
    res.extend(entry(RT_VERSION, 1, 0x0030, LANGUAGE, &info));
    res
}

enum Value<'a> {
    None,
    Text(&'a str),
    Binary(&'a [u8]),
}

/// One block of a version resource: its size, the size and type of its value, its key, its value and the blocks in it,
/// each starting on a 32-bit boundary.
fn block(key: &str, value: Value, children: &[Vec<u8>]) -> Vec<u8> {
    let (value_len, kind, bytes) = match value {
        Value::None => (0, 1u16, Vec::new()),
        Value::Text(text) => {
            let bytes = utf16z(text);
            (bytes.len() / 2, 1, bytes)
        }
        Value::Binary(data) => (data.len(), 0, data.to_vec()),
    };
    let mut out = vec![0, 0];
    out.extend(u16::try_from(value_len).expect("a short value").to_le_bytes());
    out.extend(kind.to_le_bytes());
    out.extend(utf16z(key));
    pad(&mut out);
    out.extend(bytes);
    for child in children {
        pad(&mut out);
        out.extend(child);
    }
    let len = u16::try_from(out.len()).expect("a version resource under 64 KiB");
    out[..2].copy_from_slice(&len.to_le_bytes());
    out
}

/// One resource in a `.res` file: its header with a numbered type and name, then its data.
fn entry(kind: u16, name: u16, flags: u16, language: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(u32::try_from(data.len()).expect("a small resource").to_le_bytes());
    out.extend(32u32.to_le_bytes());
    for word in [0xFFFF, kind, 0xFFFF, name] {
        out.extend(u16::to_le_bytes(word));
    }
    out.extend(0u32.to_le_bytes());
    out.extend(flags.to_le_bytes());
    out.extend(language.to_le_bytes());
    out.extend([0u8; 8]);
    out.extend(data);
    pad(&mut out);
    out
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn pad(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}
