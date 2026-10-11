//! Line framing: one JSON object per line (UTF-8, `\n`).

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Bytes in one kibibyte.
const KIB: usize = 1024;

/// Longest line in bytes, without its `\n`: 1 MiB. A peer that sends a
/// longer line is broken, and the reader ends the stream.
pub const MAX_LINE_BYTES: usize = KIB * KIB;

const LINE_END: u8 = b'\n';

/// Why a line was not read or not written. It never holds line content.
#[derive(Debug)]
pub enum FrameError {
    /// The line is longer than [`MAX_LINE_BYTES`].
    TooLong,
    /// The line is not UTF-8.
    NotUtf8,
    /// The line is not a known JSON line of the protocol. Only the error
    /// category is kept: the full error text can quote the line.
    Json(serde_json::error::Category),
    /// The line is a known line, but not one for this helper or for this
    /// point of the session: another protocol, or a line before `Hello`.
    Unexpected,
    /// The pipe failed.
    Io(std::io::ErrorKind),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "a line is longer than {MAX_LINE_BYTES} bytes"),
            Self::NotUtf8 => f.write_str("a line is not UTF-8"),
            Self::Json(category) => write!(f, "a line is not a known JSON line ({category:?})"),
            Self::Unexpected => f.write_str("a line does not belong here"),
            Self::Io(kind) => write!(f, "the pipe failed ({kind})"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// One line with its `\n`.
pub fn encode<T: Serialize>(line: &T) -> Result<String, FrameError> {
    let mut text =
        serde_json::to_string(line).map_err(|error| FrameError::Json(error.classify()))?;
    if text.len() > MAX_LINE_BYTES {
        return Err(FrameError::TooLong);
    }
    text.push(char::from(LINE_END));
    Ok(text)
}

/// Read one line. `line` has no `\n`.
pub fn decode<T: DeserializeOwned>(line: &str) -> Result<T, FrameError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(FrameError::TooLong);
    }
    serde_json::from_str(line).map_err(|error| FrameError::Json(error.classify()))
}

/// Write one line and flush it, so the peer reads it now.
pub async fn write_line<W, T>(writer: &mut W, line: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let text = encode(line)?;
    writer.write_all(text.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads lines of at most [`MAX_LINE_BYTES`] from a pipe.
///
/// It keeps a part of a line between calls, so [`Self::next_line`] is
/// cancel safe: a `select!` can drop the call and call it again.
pub struct FrameReader<R> {
    reader: R,
    partial: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    #[must_use]
    pub const fn new(reader: R) -> Self {
        Self {
            reader,
            partial: Vec::new(),
        }
    }

    /// The next line without its `\n`. `None` at the end of the stream. A
    /// part of a line at the end of the stream is dropped: the peer stopped
    /// in the middle of a write.
    pub async fn next_line(&mut self) -> Result<Option<String>, FrameError> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                self.partial.clear();
                return Ok(None);
            }
            let end = available.iter().position(|byte| *byte == LINE_END);
            let taken = end.unwrap_or(available.len());
            if self.partial.len() + taken > MAX_LINE_BYTES {
                return Err(FrameError::TooLong);
            }
            self.partial.extend_from_slice(&available[..taken]);
            self.reader.consume(taken + usize::from(end.is_some()));
            if end.is_some() {
                let line = std::mem::take(&mut self.partial);
                return String::from_utf8(line)
                    .map(Some)
                    .map_err(|_| FrameError::NotUtf8);
            }
        }
    }

    /// The pipe, without the part of a line that was already read.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// The next line as a wire value. `None` at the end of the stream.
    pub async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        match self.next_line().await? {
            Some(line) => decode(&line).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, BufReader};

    use super::*;
    use crate::{AppLine, HelperLine};

    /// Small pipe buffer: a line arrives in several parts.
    const PIPE_BYTES: usize = 16;

    #[tokio::test]
    async fn lines_arrive_whole_through_a_small_pipe() {
        let (mut tx, rx) = tokio::io::duplex(PIPE_BYTES);
        let mut reader = FrameReader::new(BufReader::new(rx));
        let writer = tokio::spawn(async move {
            write_line(&mut tx, &HelperLine::Ack { id: 1 })
                .await
                .expect("first line");
            write_line(&mut tx, &HelperLine::Ack { id: 2 })
                .await
                .expect("second line");
        });
        assert_eq!(
            reader.next::<HelperLine>().await.expect("read"),
            Some(HelperLine::Ack { id: 1 })
        );
        assert_eq!(
            reader.next::<HelperLine>().await.expect("read"),
            Some(HelperLine::Ack { id: 2 })
        );
        writer.await.expect("writer");
        assert_eq!(reader.next::<HelperLine>().await.expect("end"), None);
    }

    #[tokio::test]
    async fn a_part_of_a_line_at_the_end_of_the_stream_is_dropped() {
        let (mut tx, rx) = tokio::io::duplex(KIB);
        let mut reader = FrameReader::new(BufReader::new(rx));
        tx.write_all(b"{\"type\":\"shutdown\"}\n{\"type\":\"shut")
            .await
            .expect("write");
        drop(tx);
        assert_eq!(
            reader.next::<AppLine>().await.expect("read"),
            Some(AppLine::Shutdown)
        );
        assert_eq!(reader.next::<AppLine>().await.expect("end"), None);
    }

    #[tokio::test]
    async fn a_line_over_the_limit_ends_the_stream_with_an_error() {
        let (mut tx, rx) = tokio::io::duplex(KIB);
        let mut reader = FrameReader::new(BufReader::new(rx));
        let writer = tokio::spawn(async move {
            let chunk = [b'a'; KIB];
            // One byte over the limit, and no line end.
            for _ in 0..KIB {
                if tx.write_all(&chunk).await.is_err() {
                    return;
                }
            }
            let _ = tx.write_all(b"a").await;
        });
        assert!(matches!(reader.next_line().await, Err(FrameError::TooLong)));
        drop(reader);
        writer.await.expect("writer");
    }

    #[tokio::test]
    async fn a_line_at_the_limit_is_read() {
        let (mut tx, rx) = tokio::io::duplex(KIB);
        let mut reader = FrameReader::new(BufReader::new(rx));
        let writer = tokio::spawn(async move {
            let chunk = [b'a'; KIB];
            for _ in 0..KIB {
                tx.write_all(&chunk).await.expect("chunk");
            }
            tx.write_all(b"\n").await.expect("line end");
        });
        let line = reader.next_line().await.expect("read").expect("a line");
        assert_eq!(line.len(), MAX_LINE_BYTES);
        writer.await.expect("writer");
    }

    #[test]
    fn a_line_that_is_too_long_is_not_encoded() {
        let long = "a".repeat(MAX_LINE_BYTES);
        assert!(matches!(encode(&long), Err(FrameError::TooLong)));
    }

    #[tokio::test]
    async fn bytes_that_are_not_utf8_are_an_error() {
        let (mut tx, rx) = tokio::io::duplex(KIB);
        let mut reader = FrameReader::new(BufReader::new(rx));
        tx.write_all(&[0xff, 0xfe, LINE_END]).await.expect("write");
        assert!(matches!(reader.next_line().await, Err(FrameError::NotUtf8)));
    }

    /// The error text of a bad line never quotes the line.
    #[test]
    fn a_decode_error_does_not_quote_the_line() {
        let error = decode::<AppLine>(r#"{"type":"secret-fixture-91"}"#).expect_err("unknown type");
        assert!(!error.to_string().contains("secret-fixture-91"), "{error}");
    }
}
