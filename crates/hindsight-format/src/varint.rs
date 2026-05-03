// SPDX-License-Identifier: Apache-2.0

//! Unsigned LEB128 varint encoding, plus a ZigZag-encoded signed variant.

use std::io::{self, Write};

pub fn write_uvarint<W: Write + ?Sized>(w: &mut W, mut value: u64) -> io::Result<usize> {
    let mut written = 0;
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        w.write_all(&[byte])?;
        written += 1;
        if value == 0 {
            return Ok(written);
        }
    }
}

pub fn uvarint_len(mut value: u64) -> usize {
    let mut count = 1;
    while value >= 0x80 {
        value >>= 7;
        count += 1;
    }
    count
}

pub fn write_svarint<W: Write + ?Sized>(w: &mut W, value: i64) -> io::Result<usize> {
    let zigzag = ((value as u64) << 1) ^ ((value >> 63) as u64);
    write_uvarint(w, zigzag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(value: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        write_uvarint(&mut buf, value).unwrap();
        buf
    }

    fn encode_signed(value: i64) -> Vec<u8> {
        let mut buf = Vec::new();
        write_svarint(&mut buf, value).unwrap();
        buf
    }

    #[test]
    fn varint_zero_is_one_byte() {
        assert_eq!(encode(0), vec![0x00]);
    }

    #[test]
    fn varint_seven_bit_boundary() {
        assert_eq!(encode(127), vec![0x7F]);
        assert_eq!(encode(128), vec![0x80, 0x01]);
    }

    #[test]
    fn varint_two_byte_examples() {
        assert_eq!(encode(300), vec![0xAC, 0x02]);
        assert_eq!(encode(16383), vec![0xFF, 0x7F]);
        assert_eq!(encode(16384), vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn varint_max_u64_is_ten_bytes() {
        assert_eq!(encode(u64::MAX).len(), 10);
    }

    #[test]
    fn varint_len_matches_actual_length() {
        for value in [0, 1, 127, 128, 16383, 16384, 1 << 30, u64::MAX] {
            assert_eq!(uvarint_len(value), encode(value).len(), "value={value}");
        }
    }

    #[test]
    fn signed_varint_zigzag_pattern() {
        assert_eq!(encode_signed(0), vec![0x00]);
        assert_eq!(encode_signed(-1), vec![0x01]);
        assert_eq!(encode_signed(1), vec![0x02]);
        assert_eq!(encode_signed(-2), vec![0x03]);
        assert_eq!(encode_signed(2), vec![0x04]);
    }

    #[test]
    fn signed_varint_handles_i64_min() {
        // ZigZag(i64::MIN) = u64::MAX, should encode without panic.
        let bytes = encode_signed(i64::MIN);
        assert_eq!(bytes.len(), 10);
    }
}
