//! Строки протокола: кадр — одна строка JSON, кончающаяся переводом строки.

use std::error::Error;
use std::fmt;
use std::io;

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Лимит входящей строки сервера без перевода строки: 1 МиБ.
pub const REQUEST_LINE_LIMIT: usize = 1 << 20;

/// Лимит строки, которую принимает клиент: 64 МиБ. Ответ сервера (дерево, снимок) лимитом
/// сервера не ограничен.
pub const RESPONSE_LINE_LIMIT: usize = 64 << 20;

/// Строка не прочитана.
#[derive(Debug)]
pub enum LineError {
    /// Строка длиннее лимита до перевода строки. Остаток строки не читается.
    TooLong { limit: usize },
    /// Поток кончился посреди строки, после `len` байт без перевода строки.
    Truncated { len: usize },
    /// Ошибка чтения.
    Io(io::Error),
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { limit } => write!(f, "строка длиннее {limit} байт"),
            Self::Truncated { len } => {
                write!(f, "поток оборвался посреди строки после {len} байт")
            }
            Self::Io(error) => write!(f, "чтение не удалось: {error}"),
        }
    }
}

impl Error for LineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TooLong { .. } | Self::Truncated { .. } => None,
        }
    }
}

/// Читатель строк с лимитом длины.
///
/// Строка копится в читателе не дальше лимита: байты, которые его превышают, не забираются
/// из потока. После ошибки поток не годен для следующей строки, соединение закрывается.
#[derive(Debug)]
pub struct LineReader<R> {
    reader: R,
    limit: usize,
    line: Vec<u8>,
    /// Последний вызов вернул строку: следующий начинает новую.
    returned: bool,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    /// Читатель строк не длиннее `limit` байт без перевода строки.
    pub fn new(reader: R, limit: usize) -> Self {
        Self {
            reader,
            limit,
            line: Vec::new(),
            returned: false,
        }
    }

    /// Следующая строка без перевода строки, `None` — поток кончился на границе строк.
    ///
    /// Отмена безопасна: прочитанная часть строки остаётся в читателе, следующий вызов
    /// продолжает её.
    pub async fn next_line(&mut self) -> Result<Option<&[u8]>, LineError> {
        if self.returned {
            self.line.clear();
            self.returned = false;
        }
        loop {
            let available = self.reader.fill_buf().await.map_err(LineError::Io)?;
            if available.is_empty() {
                return match self.line.len() {
                    0 => Ok(None),
                    len => Err(LineError::Truncated { len }),
                };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let chunk = match newline {
                Some(end) => &available[..end],
                None => available,
            };
            if chunk.len() > self.limit - self.line.len() {
                return Err(LineError::TooLong { limit: self.limit });
            }
            self.line.extend_from_slice(chunk);
            let used = chunk.len() + usize::from(newline.is_some());
            self.reader.consume(used);
            if newline.is_some() {
                self.returned = true;
                return Ok(Some(&self.line));
            }
        }
    }
}

/// Строка кадра: компактный JSON и перевод строки. Компактный JSON перевода строки
/// не содержит: в строках он экранируется.
pub fn encode_line<T: Serialize>(frame: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// Поток из заданных кусков: каждый кусок — одно заполнение буфера.
    struct Chunks {
        chunks: VecDeque<Vec<u8>>,
        offset: usize,
        /// Сколько кусков открыто чтением.
        opened: usize,
        /// Сколько байт забрано.
        consumed: usize,
    }

    impl Chunks {
        fn new<const N: usize>(chunks: [&[u8]; N]) -> Self {
            Self {
                chunks: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
                offset: 0,
                opened: 0,
                consumed: 0,
            }
        }
    }

    impl tokio::io::AsyncRead for Chunks {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("читается только через AsyncBufRead")))
        }
    }

    impl AsyncBufRead for Chunks {
        fn poll_fill_buf(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
            let this = self.get_mut();
            if this.offset == 0 && !this.chunks.is_empty() {
                this.opened += 1;
            }
            Poll::Ready(Ok(match this.chunks.front() {
                Some(chunk) => &chunk[this.offset..],
                None => &[],
            }))
        }

        fn consume(self: Pin<&mut Self>, amount: usize) {
            let this = self.get_mut();
            this.consumed += amount;
            this.offset += amount;
            if this
                .chunks
                .front()
                .is_some_and(|chunk| this.offset == chunk.len())
            {
                this.chunks.pop_front();
                this.offset = 0;
            }
        }
    }

    fn line_of(len: usize, newline: bool) -> Vec<u8> {
        let mut line = vec![b'a'; len];
        if newline {
            line.push(b'\n');
        }
        line
    }

    async fn lines(input: &[u8], limit: usize) -> Vec<Result<Vec<u8>, String>> {
        let mut reader = LineReader::new(input, limit);
        let mut lines = Vec::new();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => lines.push(Ok(line.to_vec())),
                Ok(None) => return lines,
                Err(error) => {
                    lines.push(Err(error.to_string()));
                    return lines;
                }
            }
        }
    }

    #[tokio::test]
    async fn line_of_exactly_limit_is_read() {
        let input = line_of(REQUEST_LINE_LIMIT, true);
        let mut reader = LineReader::new(BufReader::new(&input[..]), REQUEST_LINE_LIMIT);
        let line = reader.next_line().await.unwrap().unwrap();
        assert_eq!(line.len(), REQUEST_LINE_LIMIT);
        assert!(reader.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn line_over_limit_with_newline_is_too_long() {
        let input = line_of(REQUEST_LINE_LIMIT + 1, true);
        let mut reader = LineReader::new(BufReader::new(&input[..]), REQUEST_LINE_LIMIT);
        assert!(matches!(
            reader.next_line().await,
            Err(LineError::TooLong {
                limit: REQUEST_LINE_LIMIT
            })
        ));
    }

    #[tokio::test]
    async fn line_over_limit_without_newline_is_too_long() {
        let input = line_of(REQUEST_LINE_LIMIT + 1, false);
        let mut reader = LineReader::new(BufReader::new(&input[..]), REQUEST_LINE_LIMIT);
        assert!(matches!(
            reader.next_line().await,
            Err(LineError::TooLong { .. })
        ));
    }

    #[tokio::test]
    async fn line_of_exactly_limit_without_newline_is_truncated() {
        let input = line_of(REQUEST_LINE_LIMIT, false);
        let mut reader = LineReader::new(BufReader::new(&input[..]), REQUEST_LINE_LIMIT);
        assert!(matches!(
            reader.next_line().await,
            Err(LineError::Truncated {
                len: REQUEST_LINE_LIMIT
            })
        ));
    }

    #[tokio::test]
    async fn endless_line_stops_at_limit() {
        let mut reader = LineReader::new(BufReader::new(tokio::io::repeat(b'a')), 4096);
        assert!(matches!(
            reader.next_line().await,
            Err(LineError::TooLong { limit: 4096 })
        ));
    }

    #[tokio::test]
    async fn bytes_over_limit_are_not_taken_from_stream() {
        let chunk = [b'a'; 10];
        let mut reader = LineReader::new(Chunks::new([&chunk; 8]), 35);
        assert!(matches!(
            reader.next_line().await,
            Err(LineError::TooLong { limit: 35 })
        ));
        assert_eq!(reader.line.len(), 30);
        assert_eq!(reader.reader.consumed, 30);
        assert_eq!(reader.reader.chunks.len(), 5);
    }

    #[tokio::test]
    async fn client_limit_is_64_mib() {
        assert_eq!(RESPONSE_LINE_LIMIT, 64 * 1024 * 1024);
        let exact = tokio::io::repeat(b'a')
            .take(RESPONSE_LINE_LIMIT as u64)
            .chain(&b"\n"[..]);
        let mut reader = LineReader::new(
            BufReader::with_capacity(1 << 20, exact),
            RESPONSE_LINE_LIMIT,
        );
        assert_eq!(
            reader.next_line().await.unwrap().map(<[u8]>::len),
            Some(RESPONSE_LINE_LIMIT)
        );
        let over = tokio::io::repeat(b'a')
            .take(RESPONSE_LINE_LIMIT as u64 + 1)
            .chain(&b"\n"[..]);
        let mut reader =
            LineReader::new(BufReader::with_capacity(1 << 20, over), RESPONSE_LINE_LIMIT);
        assert!(matches!(
            reader.next_line().await,
            Err(LineError::TooLong {
                limit: RESPONSE_LINE_LIMIT
            })
        ));
    }

    #[tokio::test]
    async fn empty_line_is_returned_empty() {
        assert_eq!(lines(b"\n", 16).await, vec![Ok(Vec::new())]);
        assert_eq!(
            lines(b"\n\n{}\n", 16).await,
            vec![Ok(Vec::new()), Ok(Vec::new()), Ok(b"{}".to_vec())]
        );
    }

    #[tokio::test]
    async fn empty_stream_ends_without_line() {
        assert_eq!(lines(b"", 16).await, Vec::new());
    }

    #[tokio::test]
    async fn several_frames_in_one_read() {
        let mut reader = LineReader::new(
            Chunks::new([b"{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n", b"{\"id\":4}\n"]),
            64,
        );
        for expected in [&b"{\"id\":1}"[..], b"{\"id\":2}", b"{\"id\":3}"] {
            assert_eq!(reader.next_line().await.unwrap(), Some(expected));
            assert_eq!(reader.reader.opened, 1);
        }
        assert_eq!(reader.next_line().await.unwrap(), Some(&b"{\"id\":4}"[..]));
        assert_eq!(reader.reader.opened, 2);
        assert!(reader.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn line_split_across_reads_is_joined() {
        let input = b"{\"id\":1,\"hello\":{\"protocol\":2}}\n{\"id\":2}\n";
        let mut reader = LineReader::new(BufReader::with_capacity(8, &input[..]), 64);
        assert_eq!(
            reader.next_line().await.unwrap(),
            Some(&b"{\"id\":1,\"hello\":{\"protocol\":2}}"[..])
        );
        assert_eq!(reader.next_line().await.unwrap(), Some(&b"{\"id\":2}"[..]));
        assert!(reader.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stream_cut_mid_line_is_truncated() {
        assert_eq!(
            lines(b"{\"id\":1}\n{\"id\":2", 64).await,
            vec![
                Ok(b"{\"id\":1}".to_vec()),
                Err(LineError::Truncated { len: 7 }.to_string())
            ]
        );
    }

    #[tokio::test]
    async fn cancelled_read_keeps_partial_line() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut reader = LineReader::new(BufReader::new(server), 64);
        client.write_all(b"{\"id\":").await.unwrap();
        tokio::select! {
            biased;
            line = reader.next_line() => panic!("строка без перевода строки: {line:?}"),
            () = std::future::ready(()) => {}
        }
        assert_eq!(reader.line, b"{\"id\":");
        client.write_all(b"7}\n").await.unwrap();
        assert_eq!(reader.next_line().await.unwrap(), Some(&b"{\"id\":7}"[..]));
    }

    #[tokio::test]
    async fn encoded_frame_is_one_line() {
        let frame = serde_json::json!({"id": 1, "text": "две\nстроки\r\n"});
        let line = encode_line(&frame).unwrap();
        assert_eq!(line.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(line.last(), Some(&b'\n'));
        let mut reader = LineReader::new(&line[..], REQUEST_LINE_LIMIT);
        let read = reader.next_line().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(read).unwrap(),
            frame
        );
    }
}
