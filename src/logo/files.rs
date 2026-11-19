//! PNG and ICO files of the logo, for `assets\`. Only the tests use this: the app draws its icons at run time, and
//! `build.rs` copies `assets\tiny-skill-sync.ico` into the exes as it is.
//!
//! The PNG writer compresses with deflate's fixed codes and a simple LZ77 search: the logo's large flat areas compress
//! well with that, and the output is the same on every run, so a test can compare the files byte for byte.

/// A PNG file of straight-alpha `0xAARRGGBB` pixels, `size` by `size`.
pub fn png(pixels: &[u32], size: u32) -> Vec<u8> {
    let stride = size as usize * 4;
    let rgba: Vec<u8> = pixels.iter().flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]).collect();
    let mut raw = Vec::with_capacity(rgba.len() + size as usize);
    for (r, row) in rgba.chunks(stride).enumerate() {
        let above = if r == 0 { None } else { Some(&rgba[(r - 1) * stride..r * stride]) };
        // Of None, Sub and Up, the filter whose bytes add up to the least, as PNG encoders usually choose.
        let candidates: [(u8, Vec<u8>); 3] = [
            (0, row.to_vec()),
            (1, row.iter().enumerate().map(|(i, &b)| b.wrapping_sub(if i >= 4 { row[i - 4] } else { 0 })).collect()),
            (2, row.iter().enumerate().map(|(i, &b)| b.wrapping_sub(above.map_or(0, |a| a[i]))).collect()),
        ];
        let cost = |bytes: &[u8]| bytes.iter().map(|&b| (b as i8).unsigned_abs() as u32).sum::<u32>();
        let (filter, bytes) = candidates.into_iter().min_by_key(|(_, bytes)| cost(bytes)).expect("three filters");
        raw.push(filter);
        raw.extend(bytes);
    }
    let mut ihdr = Vec::new();
    ihdr.extend(size.to_be_bytes());
    ihdr.extend(size.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// An ICO file of PNG images, given with their sizes.
pub fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend([0u16, 1, images.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in images {
        // 256 is written as 0.
        let side = (*size % 256) as u8;
        out.extend([side, side, 0, 0]);
        out.extend([1u16, 32].iter().flat_map(|v| v.to_le_bytes()));
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend(data);
    }
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A zlib stream: one deflate block with the fixed codes, then the Adler-32 of the data.
fn zlib(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32 * 1024;
    const LONGEST: usize = 258;
    const NONE: usize = usize::MAX;
    let mut bits = Bits::default();
    bits.put(1, 1); // the last block
    bits.put(1, 2); // fixed codes
    let hash = |i: usize| (((data[i] as usize) << 10) ^ ((data[i + 1] as usize) << 5) ^ data[i + 2] as usize) & 0x7FFF;
    let mut head = vec![NONE; 1 << 15];
    let mut prev = vec![NONE; data.len()];
    let mut i = 0;
    while i < data.len() {
        let (mut best_len, mut best_dist) = (0, 0);
        if i + 2 < data.len() {
            let mut candidate = head[hash(i)];
            for _ in 0..128 {
                if candidate == NONE || i - candidate > WINDOW {
                    break;
                }
                let len = (0..LONGEST.min(data.len() - i)).take_while(|&k| data[candidate + k] == data[i + k]).count();
                if len > best_len {
                    (best_len, best_dist) = (len, i - candidate);
                }
                candidate = prev[candidate];
            }
        }
        let step = if best_len >= 3 {
            bits.length(best_len);
            bits.distance(best_dist);
            best_len
        } else {
            bits.literal(data[i] as u32);
            1
        };
        // Every position it passed can start a later match, except the last two, which have no three bytes to hash.
        for (k, link) in prev.iter_mut().enumerate().take((i + step).min(data.len().saturating_sub(2))).skip(i) {
            let h = hash(k);
            *link = head[h];
            head[h] = k;
        }
        i += step;
    }
    bits.literal(256);
    let mut out = vec![0x78, 0xDA];
    out.extend(bits.finish());
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend(((b << 16) | a).to_be_bytes());
    out
}

#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u32,
    count: u32,
}

impl Bits {
    /// `n` bits of `value`, lowest first.
    fn put(&mut self, value: u32, n: u32) {
        self.acc |= value << self.count;
        self.count += n;
        while self.count >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.count -= 8;
        }
    }

    /// A Huffman code of `n` bits, which deflate writes highest bit first.
    fn code(&mut self, code: u32, n: u32) {
        self.put(code.reverse_bits() >> (32 - n), n);
    }

    fn literal(&mut self, v: u32) {
        match v {
            0..=143 => self.code(0x30 + v, 8),
            144..=255 => self.code(0x190 + v - 144, 9),
            256..=279 => self.code(v - 256, 7),
            _ => self.code(0xC0 + v - 280, 8),
        }
    }

    fn length(&mut self, len: usize) {
        const BASE: [usize; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
        const EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
        let k = BASE.iter().rposition(|&b| b <= len).expect("a length of 3 or more");
        self.literal(257 + k as u32);
        self.put((len - BASE[k]) as u32, EXTRA[k]);
    }

    fn distance(&mut self, dist: usize) {
        const BASE: [usize; 30] =
            [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
        let k = BASE.iter().rposition(|&b| b <= dist).expect("a distance of 1 or more");
        self.code(k as u32, 5);
        self.put((dist - BASE[k]) as u32, (k as u32).saturating_sub(2) / 2);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}
