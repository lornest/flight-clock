//! Random values from the OS, for pairing codes, session tokens and the
//! setup hotspot's password.

use std::io::Read;

/// Letters and digits that are hard to mix up when read off the clock's
/// screen and typed on a phone (no i/l/o/0/1).
pub const READABLE: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

pub fn bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf,
        Err(e) => panic!("no randomness source: {}", e),
    }
}

/// A uniform random number in 0..n. Plain `% n` would make some values
/// slightly likelier than others; draws from the uneven tail are thrown away.
pub fn below(n: u32) -> u32 {
    let limit = u32::MAX - u32::MAX % n;
    loop {
        let b = bytes(4);
        let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if v < limit {
            return v % n;
        }
    }
}

pub fn hex(n: usize) -> String {
    bytes(n).iter().map(|b| format!("{:02x}", b)).collect()
}

/// `len` characters drawn evenly from `alphabet`.
pub fn string(len: usize, alphabet: &[u8]) -> String {
    (0..len).map(|_| alphabet[below(alphabet.len() as u32) as usize] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_stays_in_range() {
        for _ in 0..200 {
            assert!(below(1_000_000) < 1_000_000);
        }
        assert_eq!(below(1), 0);
    }

    #[test]
    fn strings_use_only_the_alphabet() {
        let s = string(64, READABLE);
        assert_eq!(s.len(), 64);
        assert!(s.bytes().all(|b| READABLE.contains(&b)));
        assert_ne!(string(16, READABLE), string(16, READABLE));
    }
}
