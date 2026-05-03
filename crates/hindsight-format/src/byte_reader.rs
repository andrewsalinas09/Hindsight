// SPDX-License-Identifier: Apache-2.0

//! Position-tracking reader over an in-memory byte slice.
//!
//! Section parsers and value decoding both want to (a) consume primitive
//! integers / varints / fixed-size byte runs and (b) verify that the bytes
//! they parse exactly fill a length-prefixed region. `ByteReader` exposes
//! `pos()` so callers can mark a region's start and assert
//! `pos() - start == declared_length` after parsing.
//!
//! All reads return [`crate::FormatError::Truncated`] when the buffer runs
//! out, which becomes the canonical "file ended early" error path.

use crate::error::{FormatError, Result};

pub(crate) struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(FormatError::Truncated);
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn read_u16_le(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    pub(crate) fn read_u32_le(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub(crate) fn read_u64_le(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }

    /// Unsigned LEB128, paired with [`crate::varint::write_uvarint`]. Rejects
    /// encodings that overflow u64 (more than 10 bytes, or a 10th byte whose
    /// chunk would extend past bit 63).
    pub(crate) fn read_uvarint(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        for shift_step in 0..10 {
            let shift = shift_step * 7;
            let b = self.read_u8()?;
            // 10th byte (shift == 63): only bit 0 fits in u64, and the
            // continuation bit must not be set.
            if shift_step == 9 && b > 0x01 {
                return Err(FormatError::VarintOverflow);
            }
            let chunk = u64::from(b & 0x7F);
            result |= chunk << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
        }
        // Loop exited without seeing a terminator byte after 10 reads.
        Err(FormatError::VarintOverflow)
    }

    pub(crate) fn read_svarint(&mut self) -> Result<i64> {
        let z = self.read_uvarint()?;
        Ok(((z >> 1) as i64) ^ -((z & 1) as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_errors_when_truncated() {
        let mut br = ByteReader::new(&[1, 2, 3]);
        assert!(br.take(4).is_err());
    }

    #[test]
    fn primitive_reads() {
        let mut br = ByteReader::new(&[
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F,
        ]);
        assert_eq!(br.read_u8().unwrap(), 0x01);
        assert_eq!(br.read_u16_le().unwrap(), u16::from_le_bytes([0x02, 0x03]));
        assert_eq!(
            br.read_u32_le().unwrap(),
            u32::from_le_bytes([0x04, 0x05, 0x06, 0x07])
        );
        // Consumed 1 + 2 + 4 = 7; the buffer was 15 bytes long, so 8 remain.
        assert_eq!(br.remaining(), 8);
    }

    #[test]
    fn uvarint_round_trip_known_values() {
        let mut br = ByteReader::new(&[0x00]);
        assert_eq!(br.read_uvarint().unwrap(), 0);

        let mut br = ByteReader::new(&[0x7F]);
        assert_eq!(br.read_uvarint().unwrap(), 127);

        let mut br = ByteReader::new(&[0xAC, 0x02]);
        assert_eq!(br.read_uvarint().unwrap(), 300);

        let mut br = ByteReader::new(&[0x80, 0x80, 0x01]);
        assert_eq!(br.read_uvarint().unwrap(), 16384);
    }

    #[test]
    fn uvarint_max_u64_decodes_to_max() {
        // u64::MAX as LEB128: nine 0xFF bytes then 0x01.
        let mut bytes = vec![0xFFu8; 9];
        bytes.push(0x01);
        let mut br = ByteReader::new(&bytes);
        assert_eq!(br.read_uvarint().unwrap(), u64::MAX);
    }

    #[test]
    fn uvarint_overflow_when_tenth_byte_too_large() {
        let mut bytes = vec![0xFFu8; 9];
        bytes.push(0x02); // would shift into bit 64 -> overflow
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            br.read_uvarint(),
            Err(FormatError::VarintOverflow)
        ));
    }

    #[test]
    fn uvarint_overflow_when_continuation_persists() {
        // Ten bytes all with the continuation bit set: never terminates within
        // u64 -> overflow.
        let bytes = vec![0x80u8; 10];
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            br.read_uvarint(),
            Err(FormatError::VarintOverflow)
        ));
    }

    #[test]
    fn uvarint_truncated_yields_truncated_error() {
        let mut br = ByteReader::new(&[0x80]); // continuation bit but no follow-up byte
        assert!(matches!(br.read_uvarint(), Err(FormatError::Truncated)));
    }

    #[test]
    fn svarint_zigzag_pattern() {
        let mut br = ByteReader::new(&[0x00]);
        assert_eq!(br.read_svarint().unwrap(), 0);
        let mut br = ByteReader::new(&[0x01]);
        assert_eq!(br.read_svarint().unwrap(), -1);
        let mut br = ByteReader::new(&[0x02]);
        assert_eq!(br.read_svarint().unwrap(), 1);
    }
}
