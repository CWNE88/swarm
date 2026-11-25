//! The byte encoding `protocol`'s frames use: fixed-width big-endian
//! integers, fixed-size arrays, and `u16`-length-prefixed byte strings.
//! Nothing is delimited by a character, so no field can spill into the
//! next one, whatever it contains.
//!
//! A `Reader` ignores whatever's left once a frame's known fields are
//! read: a later version can add fields to the end of a frame and older
//! readers still understand the part they know.

use anyhow::{bail, Context, Result};

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new(tag: u8) -> Self {
        Writer { buf: vec![tag] }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn array(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    /// A byte string of at most `u16::MAX` bytes, length first. Longer
    /// is a bug in the caller (every field has a much smaller limit).
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        let len = u16::try_from(v.len()).expect("field longer than u16::MAX — every field has a smaller limit");
        self.u16(len);
        self.array(v)
    }

    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    /// The rest of the frame, unprefixed — for a frame's one trailing,
    /// variable-length payload (call audio).
    pub fn rest(&mut self, v: &[u8]) -> &mut Self {
        self.array(v)
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            bail!("frame ended early");
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u16()? as usize;
        self.take(len)
    }

    pub fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).context("text field wasn't valid UTF-8")
    }

    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.buf)
    }
}

/// Bytes as lower-case hex, two digits each.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_type_round_trips_and_trailing_bytes_are_left_alone() {
        let mut w = Writer::new(7);
        w.u8(1).u16(2).u32(3).u64(4).bool(true).array(&[9; 4]).str("tab\tand\nnewline").bytes(&[]);
        let mut bytes = w.into_bytes();
        bytes.extend_from_slice(b"a later version's extra field");
        let mut r = Reader::new(&bytes[1..]);
        assert_eq!((r.u8().unwrap(), r.u16().unwrap(), r.u32().unwrap(), r.u64().unwrap()), (1, 2, 3, 4));
        assert!(r.bool().unwrap());
        assert_eq!(r.array::<4>().unwrap(), [9; 4]);
        assert_eq!(r.str().unwrap(), "tab\tand\nnewline");
        assert_eq!(r.bytes().unwrap(), b"");
    }

    #[test]
    fn a_short_frame_is_an_error_not_a_panic() {
        let mut r = Reader::new(&[0, 10, 1, 2]);
        assert!(r.bytes().is_err());
        assert!(Reader::new(&[1]).u64().is_err());
    }
}
