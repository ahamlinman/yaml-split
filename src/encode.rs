use std::error::Error;
use std::fmt::Display;
use std::io::{self, BufRead, Read};

const MAX_UTF8_ENCODED_LEN: usize = 4;

/// A streaming UTF-8 encoder designed to pair with [`UTF16Decoder`] or
/// [`UTF32Decoder`].
pub struct UTF8Encoder<S>
where
    S: Iterator<Item = io::Result<char>>,
{
    source: S,
    remainder: Buffer<MAX_UTF8_ENCODED_LEN>,
}

impl<S> UTF8Encoder<S>
where
    S: Iterator<Item = io::Result<char>>,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            remainder: Buffer::new(),
        }
    }
}

impl<S> Read for UTF8Encoder<S>
where
    S: Iterator<Item = io::Result<char>>,
{
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;

        // First, emit the remainder of any character that a previous read could
        // not fully emit.
        if !self.remainder.is_empty() {
            let len = self.remainder.read(buf)?;
            buf = &mut buf[len..];
            written += len;
            if !self.remainder.is_empty() {
                return Ok(written);
            }
        }

        // Second, emit as much as we can directly into the destination buffer.
        while buf.len() >= MAX_UTF8_ENCODED_LEN {
            let ch = match self.source.next() {
                Some(Ok(ch)) => ch,
                Some(Err(err)) => return Err(err),
                None => return Ok(written),
            };
            let len = ch.encode_utf8(buf).len();
            buf = &mut buf[len..];
            written += len;
        }

        // Finally, emit as much as we can into the destination buffer's
        // remaining space, storing the remainder of any character that we
        // cannot fully emit at this time.
        while !buf.is_empty() {
            let ch = match self.source.next() {
                Some(Ok(ch)) => ch,
                Some(Err(err)) => return Err(err),
                None => return Ok(written),
            };

            let mut tmp = [0u8; MAX_UTF8_ENCODED_LEN];
            let char_len = ch.encode_utf8(&mut tmp).len();

            let emit_len = std::cmp::min(char_len, buf.len());
            buf[..emit_len].copy_from_slice(&tmp[..emit_len]);
            buf = &mut buf[emit_len..];
            written += emit_len;

            if buf.is_empty() {
                self.remainder.reset(&tmp[emit_len..char_len]);
            }
        }

        Ok(written)
    }
}

/// A reusable statically-sized byte buffer with non-seekable read support.
struct Buffer<const SIZE: usize> {
    buf: [u8; SIZE],
    pos: usize,
    len: usize,
}

impl<const SIZE: usize> Buffer<SIZE> {
    /// Returns a new empty buffer.
    fn new() -> Self {
        Self {
            buf: [0u8; SIZE],
            pos: 0,
            len: 0,
        }
    }

    /// Returns whether the buffer is empty; that is, whether it contains no
    /// unread content.
    fn is_empty(&self) -> bool {
        self.pos == self.len
    }

    /// Replaces any existing contents of the buffer with the provided bytes.
    /// Future reads will produce these bytes until EOF.
    ///
    /// # Panics
    ///
    /// Panics if `buf` is longer than the static size of the buffer.
    fn reset(&mut self, buf: &[u8]) {
        debug_assert!(
            buf.len() <= SIZE,
            "called Buffer::set with a slice of size {} on a Buffer of size {}",
            buf.len(),
            SIZE,
        );
        self.buf[..buf.len()].copy_from_slice(buf);
        self.pos = 0;
        self.len = buf.len();
    }
}

impl<const SIZE: usize> Read for Buffer<SIZE> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = std::cmp::min(self.len - self.pos, buf.len());
        buf[..len].copy_from_slice(&self.buf[self.pos..self.pos + len]);
        self.pos += len;
        Ok(len)
    }
}

/// Represents the endianness of UTF-16 or UTF-32 text.
pub enum Endianness {
    Big,
    Little,
}

impl Endianness {
    fn decode_u16(&self, buf: [u8; 2]) -> u16 {
        match self {
            Endianness::Big => u16::from_be_bytes(buf),
            Endianness::Little => u16::from_le_bytes(buf),
        }
    }

    fn decode_u32(&self, buf: [u8; 4]) -> u32 {
        match self {
            Endianness::Big => u32::from_be_bytes(buf),
            Endianness::Little => u32::from_le_bytes(buf),
        }
    }
}

/// A streaming UTF-16 decoder.
pub struct UTF16Decoder<R>
where
    R: BufRead,
{
    source: R,
    pos: u64,
    endianness: Endianness,
    buf: Option<u16>,
}

impl<R> UTF16Decoder<R>
where
    R: BufRead,
{
    pub fn new(source: R, endianness: Endianness) -> Self {
        Self {
            source,
            pos: 0,
            endianness,
            buf: None,
        }
    }

    fn next_u16(&mut self) -> io::Result<Option<u16>> {
        match self.source.fill_buf() {
            Ok(buf) if buf.is_empty() => return Ok(None),
            Err(err) => return Err(err),
            _ => {}
        };

        let mut next = [0u8; 2];
        self.source.read_exact(&mut next)?;
        self.pos += next.len() as u64;
        Ok(Some(self.endianness.decode_u16(next)))
    }
}

impl<R> Iterator for UTF16Decoder<R>
where
    R: BufRead,
{
    type Item = io::Result<char>;

    fn next(&mut self) -> Option<Self::Item> {
        let pos = self.pos;
        let lead = match self.buf.take() {
            Some(u) => u,
            None => match self.next_u16() {
                Ok(Some(u)) => u,
                Ok(None) => return None,
                Err(err) => return Some(Err(err)),
            },
        };

        if !(0xD800..=0xDFFF).contains(&lead) {
            // SAFETY: This is not a UTF-16 surrogate, which means that the u16
            // code unit directly encodes the desired code point.
            return Some(Ok(unsafe { char::from_u32_unchecked(lead as u32) }));
        }

        if lead >= 0xDC00 {
            // Invalid: a UTF-16 trailing surrogate with no leading surrogate.
            return Some(Err(InvalidUTF16Error::new(lead, pos).into()));
        }

        let pos = self.pos;
        let trail = match self.next_u16() {
            Ok(Some(u)) => u,
            Ok(None) => return Some(Err(io::ErrorKind::UnexpectedEof.into())),
            Err(err) => return Some(Err(err)),
        };
        if !(0xDC00..=0xDFFF).contains(&trail) {
            // Invalid: we needed a trailing surrogate and didn't get one. We'll
            // try to decode this as a leading code unit on the next iteration.
            self.buf = Some(trail);
            return Some(Err(InvalidUTF16Error::new(trail, pos).into()));
        }

        // At this point, we are confident that we have valid leading and
        // trailing surrogates, and can decode them into the correct code point.
        let ch = 0x1_0000 + (((lead - 0xD800) as u32) << 10 | (trail - 0xDC00) as u32);
        // SAFETY: We have confirmed that the surrogate pair is valid.
        Some(Ok(unsafe { char::from_u32_unchecked(ch as u32) }))
    }
}

#[derive(Debug)]
struct InvalidUTF16Error {
    unit: u16,
    pos: u64,
}

impl InvalidUTF16Error {
    fn new(unit: u16, pos: u64) -> Self {
        Self { unit, pos }
    }
}

impl From<InvalidUTF16Error> for io::Error {
    fn from(err: InvalidUTF16Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, err)
    }
}

impl Error for InvalidUTF16Error {}

impl Display for InvalidUTF16Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid or unexpected UTF-16 code unit {:x} at byte {}",
            self.unit, self.pos,
        )
    }
}

/// A streaming UTF-32 decoder.
pub struct UTF32Decoder<R>
where
    R: BufRead,
{
    source: R,
    pos: u64,
    endianness: Endianness,
}

impl<R> UTF32Decoder<R>
where
    R: BufRead,
{
    pub fn new(source: R, endianness: Endianness) -> Self {
        Self {
            source,
            pos: 0,
            endianness,
        }
    }
}

impl<R> Iterator for UTF32Decoder<R>
where
    R: BufRead,
{
    type Item = io::Result<char>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.source.fill_buf() {
            Ok(buf) if buf.is_empty() => return None,
            Err(err) => return Some(Err(err)),
            _ => {}
        };

        let pos = self.pos;
        let mut next = [0u8; 4];
        if let Err(err) = self.source.read_exact(&mut next) {
            return Some(Err(err));
        }
        self.pos += next.len() as u64;

        let unit = self.endianness.decode_u32(next);
        Some(match char::from_u32(unit) {
            Some(ch) => Ok(ch),
            None => Err(InvalidUTF32Error::new(unit, pos).into()),
        })
    }
}

#[derive(Debug)]
struct InvalidUTF32Error {
    unit: u32,
    pos: u64,
}

impl InvalidUTF32Error {
    fn new(unit: u32, pos: u64) -> Self {
        Self { unit, pos }
    }
}

impl From<InvalidUTF32Error> for io::Error {
    fn from(err: InvalidUTF32Error) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, err)
    }
}

impl Error for InvalidUTF32Error {}

impl Display for InvalidUTF32Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid UTF-32 code unit {:x} at byte {}",
            self.unit, self.pos,
        )
    }
}
