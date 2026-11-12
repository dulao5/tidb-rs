//! The parts of Go's `hash/crc32` TiDB uses.

/// The IEEE polynomial (reversed), `crc32.IEEE`.
const IEEE: u32 = 0xedb8_8320;

/// The table of the IEEE polynomial, `crc32.IEEETable` (simple 256-entry form; Go uses
/// slicing-by-8 or hardware CRC, which give the same checksum).
const IEEE_TABLE: [u32; 256] = make_table(IEEE);

// go-std: hash/crc32.simpleMakeTable L1
const fn make_table(poly: u32) -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ poly;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        t[i] = crc;
        i += 1;
    }
    t
}

/// `crc32.ChecksumIEEE(data)`: the CRC-32 checksum of `data` with the IEEE polynomial.
// go-std: hash/crc32.ChecksumIEEE L2 (table-driven; same checksum as Go's arch-specific paths)
pub fn checksum_ieee(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc = IEEE_TABLE[((crc as u8) ^ b) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::checksum_ieee;

    #[test]
    fn known_values() {
        assert_eq!(checksum_ieee(b""), 0);
        assert_eq!(checksum_ieee(b"MySQL"), 3_259_397_556);
        assert_eq!(checksum_ieee(b"123456789"), 0xcbf4_3926);
    }
}
