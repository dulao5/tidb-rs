//! Primitive encoders/decoders for packet payloads (little-endian integers, length-encoded
//! integers and strings), matching TiDB's `server/internal/dump` and `server/internal/util`.

use crate::error::SqlError;

/// Appends a length-encoded integer.
pub fn put_lenenc_int(buf: &mut Vec<u8>, n: u64) {
    if n <= 250 {
        buf.push(n as u8);
    } else if n <= 0xffff {
        buf.push(0xfc);
        buf.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= 0xff_ffff {
        buf.push(0xfd);
        buf.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
    } else {
        buf.push(0xfe);
        buf.extend_from_slice(&n.to_le_bytes());
    }
}

/// Appends a length-encoded string.
pub fn put_lenenc_str(buf: &mut Vec<u8>, s: &[u8]) {
    put_lenenc_int(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

/// Appends a NUL-terminated string.
pub fn put_nul_str(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(s);
    buf.push(0);
}

/// Bounds-checked cursor over a packet payload. Every read past the end yields
/// [`SqlError::malformed`], so malformed client input never panics.
#[derive(Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Returns the unread bytes without consuming them.
    pub fn peek_rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Consumes and returns all unread bytes.
    pub fn rest(&mut self) -> &'a [u8] {
        let rest = &self.data[self.pos..];
        self.pos = self.data.len();
        rest
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], SqlError> {
        if n > self.remaining() {
            return Err(SqlError::malformed());
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), SqlError> {
        self.take(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8, SqlError> {
        Ok(self.take(1)?[0])
    }

    pub fn peek_u8(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    pub fn u16(&mut self) -> Result<u16, SqlError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, SqlError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, SqlError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], SqlError> {
        let s = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(s);
        Ok(out)
    }

    /// Reads a NUL-terminated string (the NUL is consumed, not returned).
    pub fn nul_str(&mut self) -> Result<&'a [u8], SqlError> {
        let rest = self.peek_rest();
        let n = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(SqlError::malformed)?;
        let s = &rest[..n];
        self.pos += n + 1;
        Ok(s)
    }

    /// Reads a length-encoded integer; `None` is the NULL marker `0xfb`.
    pub fn lenenc_int(&mut self) -> Result<Option<u64>, SqlError> {
        let first = self.u8()?;
        Ok(Some(match first {
            0xfb => return Ok(None),
            0xfc => u64::from(self.u16()?),
            0xfd => {
                let b: [u8; 3] = self.array()?;
                u64::from(b[0]) | (u64::from(b[1]) << 8) | (u64::from(b[2]) << 16)
            }
            0xfe => self.u64()?,
            // 0xff is not a valid length prefix.
            0xff => return Err(SqlError::malformed()),
            n => u64::from(n),
        }))
    }

    /// Reads a length-encoded string; `None` is the NULL marker.
    pub fn lenenc_str(&mut self) -> Result<Option<&'a [u8]>, SqlError> {
        match self.lenenc_int()? {
            None => Ok(None),
            Some(n) => {
                let n = usize::try_from(n).map_err(|_| SqlError::malformed())?;
                self.take(n).map(Some)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lenenc(n: u64) -> Vec<u8> {
        let mut b = Vec::new();
        put_lenenc_int(&mut b, n);
        b
    }

    #[test]
    fn lenenc_int_layout_matches_tidb_dump() {
        // Boundaries from TiDB's dump.LengthEncodedInt.
        assert_eq!(lenenc(0), [0x00]);
        assert_eq!(lenenc(250), [0xfa]);
        assert_eq!(lenenc(251), [0xfc, 0xfb, 0x00]);
        assert_eq!(lenenc(0xffff), [0xfc, 0xff, 0xff]);
        assert_eq!(lenenc(0x1_0000), [0xfd, 0x00, 0x00, 0x01]);
        assert_eq!(lenenc(0xff_ffff), [0xfd, 0xff, 0xff, 0xff]);
        assert_eq!(lenenc(0x100_0000), [0xfe, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(
            lenenc(u64::MAX),
            [0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn lenenc_roundtrip() {
        for n in [
            0,
            1,
            250,
            251,
            0xffff,
            0x1_0000,
            0xff_ffff,
            0x100_0000,
            u64::MAX,
        ] {
            let b = lenenc(n);
            let mut r = Reader::new(&b);
            assert_eq!(r.lenenc_int().unwrap(), Some(n));
            assert!(r.is_empty());
        }
        assert_eq!(Reader::new(&[0xfb]).lenenc_int().unwrap(), None);
    }

    #[test]
    fn reader_rejects_truncated_input() {
        assert!(Reader::new(&[0xfe, 1, 2]).lenenc_int().is_err());
        assert!(Reader::new(&[0xfc, 1]).lenenc_int().is_err());
        assert!(Reader::new(&[]).u8().is_err());
        assert!(Reader::new(b"abc").nul_str().is_err());
        // Length far beyond the payload must not panic.
        let huge = [0xfe, 0, 0, 0, 0, 0, 0, 0, 0x80];
        assert!(Reader::new(&huge).lenenc_str().is_err());
        let mut r = Reader::new(b"ab\0cd");
        assert_eq!(r.nul_str().unwrap(), b"ab");
        assert_eq!(r.rest(), b"cd");
    }
}
