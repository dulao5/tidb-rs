//! go-file: pkg/server/internal/dump/dump.go
//!
//! The byte-level writers of the MySQL protocol (length-encoded integers and strings, the
//! binary forms of TIME and DATETIME). Go returns the grown buffer; here the buffer is
//! appended to in place.

use tidb_base::gostd::time::Duration;
use tidb_base::mysql::{TypeDate, TypeDatetime, TypeTimestamp};
use tidb_types::time::Time;

/// LengthEncodedString dumps a string as length encoded byte slice.
// go: LengthEncodedString L1
pub fn length_encoded_string(buffer: &mut Vec<u8>, bytes: &[u8]) {
    length_encoded_int(buffer, bytes.len() as u64);
    buffer.extend_from_slice(bytes);
}

/// LengthEncodedInt dumps an integer as length encoded byte slice.
// go: LengthEncodedInt L1
pub fn length_encoded_int(buffer: &mut Vec<u8>, n: u64) {
    match n {
        0..=250 => buffer.push(n as u8),
        251..=0xffff => buffer.extend_from_slice(&[0xfc, n as u8, (n >> 8) as u8]),
        0x10000..=0xffffff => {
            buffer.extend_from_slice(&[0xfd, n as u8, (n >> 8) as u8, (n >> 16) as u8])
        }
        _ => {
            buffer.push(0xfe);
            buffer.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// Uint16 dumps an uint16 as byte slice.
// go: Uint16 L1
pub fn uint16(buffer: &mut Vec<u8>, n: u16) {
    buffer.extend_from_slice(&n.to_le_bytes());
}

/// Uint32 dumps an uint32 as byte slice.
// go: Uint32 L1
pub fn uint32(buffer: &mut Vec<u8>, n: u32) {
    buffer.extend_from_slice(&n.to_le_bytes());
}

/// Uint64 dumps an uint64 as byte slice.
// go: Uint64 L1
pub fn uint64(buffer: &mut Vec<u8>, n: u64) {
    buffer.extend_from_slice(&n.to_le_bytes());
}

/// BinaryTime dumps a time as binary byte slice.
// go: BinaryTime L1
pub fn binary_time(dur: Duration) -> Vec<u8> {
    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    if dur == 0 {
        return vec![0];
    }
    let mut data = vec![0u8; 13];
    data[0] = 12;
    let mut dur = dur;
    if dur < 0 {
        data[1] = 1;
        dur = dur.wrapping_neg();
    }
    let days = dur / (24 * HOUR);
    dur -= days * 24 * HOUR;
    data[2] = days as u8;
    let hours = dur / HOUR;
    dur -= hours * HOUR;
    data[6] = hours as u8;
    let minutes = dur / MINUTE;
    dur -= minutes * MINUTE;
    data[7] = minutes as u8;
    let seconds = dur / SECOND;
    dur -= seconds * SECOND;
    data[8] = seconds as u8;
    if dur == 0 {
        data[0] = 8;
        data.truncate(9);
        return data;
    }
    data[9..13].copy_from_slice(&((dur / 1000) as u32).to_le_bytes());
    data
}

/// BinaryDateTime dumps a datetime as binary byte slice.
// go: BinaryDateTime L1
pub fn binary_date_time(data: &mut Vec<u8>, t: Time) {
    let (year, mon, day) = (t.year(), t.month(), t.day());
    let tp = t.tp();
    if tp == TypeTimestamp || tp == TypeDatetime {
        if t.is_zero() {
            // All zero.
            data.push(0);
        } else if t.microsecond() != 0 {
            // Has micro seconds.
            data.push(11);
            uint16(data, year as u16);
            data.extend_from_slice(&[
                mon as u8,
                day as u8,
                t.hour() as u8,
                t.minute() as u8,
                t.second() as u8,
            ]);
            uint32(data, t.microsecond() as u32);
        } else if t.hour() != 0 || t.minute() != 0 || t.second() != 0 {
            // Has HH:MM:SS
            data.push(7);
            uint16(data, year as u16);
            data.extend_from_slice(&[
                mon as u8,
                day as u8,
                t.hour() as u8,
                t.minute() as u8,
                t.second() as u8,
            ]);
        } else {
            // Only YY:MM:DD
            data.push(4);
            uint16(data, year as u16);
            data.extend_from_slice(&[mon as u8, day as u8]);
        }
    } else if tp == TypeDate {
        if t.is_zero() {
            data.push(0);
        } else {
            data.push(4);
            uint16(data, year as u16); // year
            data.extend_from_slice(&[mon as u8, day as u8]);
        }
    }
}
