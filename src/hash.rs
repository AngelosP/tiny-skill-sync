//! SHA-1 and Git blob hashing.
//!
//! Git names every file version by `sha1("blob <len>\0" + bytes)`. The same hash therefore checks a download
//! against the tree listing and a file on disk against the manifest. It is an integrity check; the transport
//! (TLS) is what authenticates the content.

#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    filled: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    pub fn new() -> Self {
        Sha1 { state: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0], block: [0; 64], filled: 0, total: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.filled > 0 {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled < 64 {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        while data.len() >= 64 {
            compress(&mut self.state, data[..64].try_into().expect("64 bytes"));
            data = &data[64..];
        }
        self.block[..data.len()].copy_from_slice(data);
        self.filled = data.len();
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bit_len = self.total.wrapping_mul(8);
        let pad_len = if self.filled < 56 { 56 - self.filled } else { 120 - self.filled };
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        self.update(&pad[..pad_len]);
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 20];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (i, chunk) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (i, &word) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
            20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6u32),
        };
        let next = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = next;
    }
    for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
        *slot = slot.wrapping_add(value);
    }
}

/// Lower-case hex, the form Git and GitHub use.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// True for a 40-character lower-case hex SHA-1.
pub fn is_sha1_hex(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Incremental Git blob hash. The length must be known up front because Git hashes it first.
pub struct BlobHasher {
    sha: Sha1,
}

impl BlobHasher {
    pub fn new(len: u64) -> Self {
        let mut sha = Sha1::new();
        sha.update(format!("blob {len}\0").as_bytes());
        BlobHasher { sha }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.sha.update(data);
    }

    pub fn finish_hex(self) -> String {
        hex(&self.sha.finish())
    }
}

pub fn blob_hex(data: &[u8]) -> String {
    let mut h = BlobHasher::new(data.len() as u64);
    h.update(data);
    h.finish_hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha1_hex(data: &[u8]) -> String {
        let mut s = Sha1::new();
        s.update(data);
        hex(&s.finish())
    }

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(b"hello\n"), "f572d396fae9206628714fb2ce00f72e94f2258f");
        let million = vec![b'a'; 1_000_000];
        assert_eq!(sha1_hex(&million), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    #[test]
    fn blob_hashes_match_git() {
        assert_eq!(blob_hex(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(blob_hex(b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(blob_hex(b"abc"), "f2ba8f84ab5c1bce84a7b441cb1959cfc7093b7f");
        let big: Vec<u8> = (0..200_000usize).map(|i| ((i * 7 + 13) % 251) as u8).collect();
        assert_eq!(blob_hex(&big), "4640ffc49ec7665e2d82c862e3cf58e13f0b7927");
    }

    #[test]
    fn chunking_does_not_change_the_hash() {
        let data: Vec<u8> = (0..1000usize).map(|i| (i % 253) as u8).collect();
        let whole = blob_hex(&data);
        for chunk in [1usize, 7, 55, 56, 63, 64, 65, 128, 999] {
            let mut h = BlobHasher::new(data.len() as u64);
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(h.finish_hex(), whole, "chunk size {chunk}");
        }
    }

    #[test]
    fn padding_edges_are_correct() {
        // Lengths around the 55/56/63/64 byte padding boundaries, checked against a straight one-shot run.
        for len in 50..70usize {
            let data = vec![0x5Au8; len];
            let mut split = Sha1::new();
            split.update(&data[..len / 2]);
            split.update(&data[len / 2..]);
            assert_eq!(hex(&split.finish()), sha1_hex(&data), "len {len}");
        }
    }

    #[test]
    fn hex_validation() {
        assert!(is_sha1_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        assert!(!is_sha1_hex("E69DE29BB2D1D6434B8B29AE775AD8C2E48C5391"));
        assert!(!is_sha1_hex("e69de29"));
        assert!(!is_sha1_hex(&"g".repeat(40)));
    }
}
