//! A resource-only PE for the Windows version reader: no code, imports, or entry point.

use crate::release::Version;

pub(super) fn image(tray: bool, digit: u8) -> (Version, Vec<u8>) {
    let res: &[u8] = if tray { include_bytes!(concat!(env!("OUT_DIR"), "/tiny-skill-sync.res")) } else { include_bytes!(concat!(env!("OUT_DIR"), "/tss.res")) };
    let word = |at| u32::from_le_bytes(res[at..at + 4].try_into().unwrap()) as usize;
    // build.rs emits the empty RES header, then the version resource.
    let at = 32 + word(36);
    let mut resource = res[at..at + word(32)].to_vec();
    let current = env!("CARGO_PKG_VERSION");
    let version = current.split('.').map(|s| format!("{digit}{}", "0".repeat(s.len() - 1))).collect::<Vec<_>>().join(".");
    let wide = |s: &str| s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    let old = wide(current);
    let new = wide(&version);
    let offsets: Vec<_> = resource.windows(old.len()).enumerate().filter_map(|(i, b)| (b == old).then_some(i)).collect();
    assert_eq!(offsets.len(), 2);
    for i in offsets {
        resource[i..i + old.len()].copy_from_slice(&new);
    }
    let numbers: Vec<u32> = version.split('.').map(|s| s.parse().unwrap()).collect();
    let fixed = resource.windows(4).position(|s| s == 0xfeef_04bdu32.to_le_bytes()).unwrap();
    for start in [fixed + 8, fixed + 16] {
        resource[start..start + 4].copy_from_slice(&(numbers[0] << 16 | numbers[1]).to_le_bytes());
        resource[start + 4..start + 8].copy_from_slice(&(numbers[2] << 16).to_le_bytes());
    }

    let tree_size = 88 + resource.len();
    let raw = tree_size.next_multiple_of(512);
    let mut pe = vec![0u8; 512 + raw];
    let put = |buf: &mut [u8], at: usize, n: u32| buf[at..at + 4].copy_from_slice(&n.to_le_bytes());
    pe[..2].copy_from_slice(b"MZ");
    put(&mut pe, 60, 64);
    pe[64..68].copy_from_slice(b"PE\0\0");
    put(&mut pe, 68, 0x0001_8664); // AMD64, one section
    put(&mut pe, 84, 0x2022_00f0); // DLL, 240-byte optional header
    let opt = 88;
    for (offset, value) in [
        (0, 0x20b),
        (8, raw as u32),
        (24, 0x400000),
        (32, 4096),
        (36, 512),
        (40, 6),
        (48, 6),
        (56, 8192),
        (60, 512),
        (68, 3),
        (108, 16),
        (128, 4096),
        (132, tree_size as u32),
    ] {
        put(&mut pe, opt + offset, value);
    }
    let section = opt + 240;
    pe[section..section + 5].copy_from_slice(b".rsrc");
    for (offset, value) in [(8, tree_size as u32), (12, 4096), (16, raw as u32), (20, 512), (36, 0x4000_0040)] {
        put(&mut pe, section + offset, value);
    }
    for (offset, id, next) in [(0, 16, 0x8000_0018), (24, 1, 0x8000_0030), (48, 0x409, 72)] {
        put(&mut pe, 512 + offset + 12, 1 << 16);
        put(&mut pe, 512 + offset + 16, id);
        put(&mut pe, 512 + offset + 20, next);
    }
    put(&mut pe, 512 + 72, 4096 + 88);
    put(&mut pe, 512 + 76, resource.len() as u32);
    pe[512 + 88..512 + tree_size].copy_from_slice(&resource);
    (version.parse().unwrap(), pe)
}
