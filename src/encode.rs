use std::cmp::min;
use std::error::Error;
use std::fmt::{Debug, Display, LowerHex};
use std::io::{self, BufRead, Read};

/// The required size of a buffer large enough to encode any `char` as UTF-8,
/// per [`char::encode_utf8`].
const MAX_UTF8_ENCODED_LEN: usize = 4;

/// Reads a YAML 1.2 stream as UTF-8 regardless of its source encoding.
///
/// A `Transcoder` detects the encoding of YAML 1.2 streams using the rules in
/// [section 5.2 of the YAML 1.2.2 specification][spec]. If it determines that
/// the stream is UTF-16 or UTF-32, it transparently re-encodes it to UTF-8 and
/// strips any initial byte order mark. Otherwise, it assumes that the input is
/// UTF-8 and reads it directly.
///
/// `Transcoder` is designed for YAML 1.2 streams. Detection and re-encoding
/// behavior for arbitrary text inputs is not well-defined.
///
/// [spec]: https://yaml.org/spec/1.2.2/#52-character-encodings
pub struct Transcoder<R>(TranscoderKind<R>)
where
    R: BufRead;

enum TranscoderKind<R>
where
    R: BufRead,
{
    Passthrough(R),
    FromUTF16(UTF8Encoder<UTF16Decoder<R>>),
    FromUTF32(UTF8Encoder<UTF32Decoder<R>>),
}

impl<R> Transcoder<R>
where
    R: BufRead,
{
    /// Creates a transcoder for the provided reader.
    pub fn new(mut reader: R) -> Self {
        // Make a best effort to read the prefix. If there is an I/O error,
        // we'll just assume that the input is UTF-8 and deal with the fallout
        // later.
        //
        // TODO: There's no guarantee that `fill_buf` will give us 4 bytes even
        // if there are 4 bytes available from the source.
        let mut prefix = [0u8; 4];
        let buf = reader.fill_buf().unwrap_or(&[]);
        let len = min(buf.len(), prefix.len());
        prefix[..len].copy_from_slice(&buf[..len]);

        // See https://yaml.org/spec/1.2.2/#52-character-encodings.
        Self(match prefix {
            [0, 0, 0xFE, 0xFF] | [0, 0, 0, _] if len == 4 => TranscoderKind::FromUTF32(
                UTF8Encoder::new(UTF32Decoder::new(reader, Endianness::Big)),
            ),
            [0xFF, 0xFE, 0, 0] | [_, 0, 0, 0] if len == 4 => TranscoderKind::FromUTF32(
                UTF8Encoder::new(UTF32Decoder::new(reader, Endianness::Little)),
            ),
            [0xFE, 0xFF, ..] | [0, _, ..] if len >= 2 => TranscoderKind::FromUTF16(
                UTF8Encoder::new(UTF16Decoder::new(reader, Endianness::Big)),
            ),
            [0xFF, 0xFE, ..] | [_, 0, ..] if len >= 2 => TranscoderKind::FromUTF16(
                UTF8Encoder::new(UTF16Decoder::new(reader, Endianness::Little)),
            ),
            _ => TranscoderKind::Passthrough(reader),
        })
    }
}

impl<R> Read for Transcoder<R>
where
    R: BufRead,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.0 {
            TranscoderKind::Passthrough(r) => r.read(buf),
            TranscoderKind::FromUTF16(r) => r.read(buf),
            TranscoderKind::FromUTF32(r) => r.read(buf),
        }
    }
}

/// A streaming UTF-8 encoder that pairs with [`UTF16Decoder`] or
/// [`UTF32Decoder`].
///
/// If the source document starts with a BOM, the encoder will skip it and
/// reading will begin with the actual text content.
struct UTF8Encoder<S>
where
    S: Iterator<Item = io::Result<char>>,
{
    source: S,
    remainder: Buffer<MAX_UTF8_ENCODED_LEN>,
    started: bool,
}

impl<S> UTF8Encoder<S>
where
    S: Iterator<Item = io::Result<char>>,
{
    fn new(source: S) -> Self {
        Self {
            source,
            remainder: Buffer::new(),
            started: false,
        }
    }

    fn next_char(&mut self) -> Option<io::Result<char>> {
        match self.started {
            true => self.source.next(),
            false => {
                self.started = true;
                match self.source.next() {
                    Some(Ok(ch)) if ch == '\u{FEFF}' => self.source.next(),
                    next => next,
                }
            }
        }
    }
}

impl<S> Read for UTF8Encoder<S>
where
    S: Iterator<Item = io::Result<char>>,
{
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;

        // First, before encoding any new characters, emit the remainder of any
        // character generated by a previous read.
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
            let ch = match self.next_char() {
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
            let ch = match self.next_char() {
                Some(Ok(ch)) => ch,
                Some(Err(err)) => return Err(err),
                None => return Ok(written),
            };

            let mut tmp = [0u8; MAX_UTF8_ENCODED_LEN];
            let char_len = ch.encode_utf8(&mut tmp).len();

            let emit_len = min(char_len, buf.len());
            buf[..emit_len].copy_from_slice(&tmp[..emit_len]);
            buf = &mut buf[emit_len..];
            written += emit_len;

            if buf.is_empty() {
                self.remainder.set(&tmp[emit_len..char_len]);
            }
        }

        Ok(written)
    }
}

/// A reusable array-backed readable buffer.
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

    /// Sets the contents of the buffer to be returned by future reads,
    /// replacing any existing contents.
    ///
    /// # Panics
    ///
    /// Panics if `buf` is longer than the static size of the buffer.
    fn set(&mut self, buf: &[u8]) {
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
        let len = min(self.len - self.pos, buf.len());
        buf[..len].copy_from_slice(&self.buf[self.pos..self.pos + len]);
        self.pos += len;
        Ok(len)
    }
}

/// A streaming UTF-16 decoder.
struct UTF16Decoder<R>
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
    fn new(source: R, endianness: Endianness) -> Self {
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
        // This is based on the implementation of `std::char::DecodeUtf16` from
        // the standard library, but is reworked for improved error handling and
        // (hopefully) a bit more readability.

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
            return Some(Err(EncodingError::new(lead, pos).into()));
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
            return Some(Err(EncodingError::new(trail, pos).into()));
        }

        // At this point, we are confident that we have valid leading and
        // trailing surrogates, and can decode them into the correct code point.
        let ch = 0x1_0000 + (((lead - 0xD800) as u32) << 10 | (trail - 0xDC00) as u32);
        // SAFETY: We have confirmed that the surrogate pair is valid.
        Some(Ok(unsafe { char::from_u32_unchecked(ch as u32) }))
    }
}

/// A streaming UTF-32 decoder.
struct UTF32Decoder<R>
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
    fn new(source: R, endianness: Endianness) -> Self {
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
            None => Err(EncodingError::new(unit, pos).into()),
        })
    }
}

/// Represents the endianness of UTF-16 or UTF-32 text.
enum Endianness {
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

/// An error in a UTF-16 or UTF-32 stream.
#[derive(Debug)]
struct EncodingError<T>
where
    T: CodeUnit,
{
    unit: T,
    pos: u64,
}

trait CodeUnit: Debug + LowerHex + Send + Sync + 'static {}

impl CodeUnit for u16 {}
impl CodeUnit for u32 {}

impl<T> EncodingError<T>
where
    T: CodeUnit,
{
    const BIT_SIZE: usize = std::mem::size_of::<T>() * 8;

    fn new(unit: T, pos: u64) -> Self {
        Self { unit, pos }
    }
}

impl<T> From<EncodingError<T>> for io::Error
where
    T: CodeUnit,
{
    fn from(err: EncodingError<T>) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, err)
    }
}

impl<T> Error for EncodingError<T> where T: CodeUnit {}

impl<T> Display for EncodingError<T>
where
    T: CodeUnit,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid or unexpected UTF-{} code unit 0x{:x} at byte {}",
            Self::BIT_SIZE,
            self.unit,
            self.pos,
        )
    }
}
