//! Dependency-free SHA-256 with streaming support.
//!
//! The hasher processes data incrementally, keeping memory usage constant
//! regardless of input size. File hashing uses an 8 KiB I/O buffer.

use anyhow::Result;
use std::fs;
use std::io::Read;
use std::path::Path;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H_INIT: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Process a single 64-byte block.
#[allow(clippy::many_single_char_names, clippy::similar_names)]
fn compress(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
        (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

/// Streaming SHA-256 hasher.
///
/// Processes data incrementally via [`update`](Sha256::update) calls,
/// then produces the final hex digest with [`finalize`](Sha256::finalize).
pub(crate) struct Sha256 {
    h: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256 {
    pub(crate) fn new() -> Self {
        Self {
            h: H_INIT,
            buffer: [0u8; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        self.total_len += data.len() as u64;
        let mut offset = 0;

        // Fill existing buffer first.
        if self.buffer_len > 0 {
            let space = 64 - self.buffer_len;
            let to_copy = data.len().min(space);
            self.buffer[self.buffer_len..self.buffer_len + to_copy]
                .copy_from_slice(&data[..to_copy]);
            self.buffer_len += to_copy;
            offset = to_copy;

            if self.buffer_len == 64 {
                compress(&mut self.h, &self.buffer);
                self.buffer_len = 0;
            }
        }

        // Process full 64-byte blocks directly from input.
        while offset + 64 <= data.len() {
            let block: &[u8; 64] = data[offset..offset + 64].try_into().unwrap();
            compress(&mut self.h, block);
            offset += 64;
        }

        // Buffer the remaining bytes.
        let remaining = data.len() - offset;
        if remaining > 0 {
            self.buffer[..remaining].copy_from_slice(&data[offset..]);
            self.buffer_len = remaining;
        }
    }

    pub(crate) fn finalize(mut self) -> String {
        let bit_len = self.total_len * 8;

        // Append the 0x80 sentinel byte.
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;

        // If the buffer cannot fit the 8-byte length, flush and start a new block.
        if self.buffer_len > 56 {
            for i in self.buffer_len..64 {
                self.buffer[i] = 0;
            }
            compress(&mut self.h, &self.buffer);
            self.buffer = [0u8; 64];
        } else {
            for i in self.buffer_len..56 {
                self.buffer[i] = 0;
            }
        }

        // Write the message bit-length into the final 8 bytes and compress.
        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut self.h, &self.buffer);

        self.h.iter().map(|word| format!("{word:08x}")).collect()
    }
}

#[cfg(test)]
/// Compute the SHA-256 digest of a byte slice, returned as a 64-char hex string.
pub(crate) fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()
}

/// Compute the SHA-256 digest of a file using streaming I/O (8 KiB buffer).
pub(crate) fn sha256_file(path: impl AsRef<Path>) -> Result<String> {
    let mut file = fs::File::open(path.as_ref())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty_input() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_known_vector() {
        // NIST test vector for "abc".
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_multi_block() {
        // NIST two-block test vector (56 bytes, forces padding into a second block).
        assert_eq!(
            sha256_bytes(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_exact_block_boundary() {
        // 55 bytes: data + 0x80 + 8-byte length fits exactly in one 64-byte block.
        let data_55 = vec![0x61u8; 55];
        let hash_55 = sha256_bytes(&data_55);
        assert_eq!(hash_55.len(), 64);

        // 56 bytes: padding spills into a second block (56 + 1 + 8 = 65 > 64).
        let data_56 = vec![0x61u8; 56];
        let hash_56 = sha256_bytes(&data_56);
        assert_eq!(hash_56.len(), 64);

        // Different inputs must produce different hashes.
        assert_ne!(hash_55, hash_56);
    }
}
