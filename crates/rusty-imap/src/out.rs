//! Output side of the session: an injected byte sink and a buffer that flushes at message
//! boundaries, so a FETCH over a big folder streams instead of building one giant reply. The
//! session never touches a socket: the driver (or a test) supplies the [`Sink`].

use async_trait::async_trait;

/// Where the session's bytes go.
#[async_trait]
pub trait Sink: Send {
    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
}

/// The peer cannot be written to any more: the session must end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

/// Buffer flushed to the sink when it grows past this many bytes (checked at message boundaries).
const FLUSH_AT: usize = 64 * 1024;

pub struct Out<'a> {
    buf: Vec<u8>,
    sink: &'a mut dyn Sink,
}

impl<'a> Out<'a> {
    pub fn new(sink: &'a mut dyn Sink) -> Out<'a> {
        Out { buf: Vec::new(), sink }
    }

    /// The buffer for the writers of [`crate::wire`].
    pub fn buf(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// `* <text>\r\n`
    pub fn untagged(&mut self, text: &str) {
        self.buf.extend_from_slice(b"* ");
        self.buf.extend_from_slice(text.as_bytes());
        self.buf.extend_from_slice(b"\r\n");
    }

    /// `<tag> <status>[ [code]] <text>\r\n`
    pub fn tagged(&mut self, tag: &str, status: &str, code: Option<&str>, text: &str) {
        self.buf.extend_from_slice(tag.as_bytes());
        self.buf.push(b' ');
        self.buf.extend_from_slice(status.as_bytes());
        if let Some(c) = code {
            self.buf.extend_from_slice(b" [");
            self.buf.extend_from_slice(c.as_bytes());
            self.buf.push(b']');
        }
        self.buf.push(b' ');
        self.buf.extend_from_slice(text.as_bytes());
        self.buf.extend_from_slice(b"\r\n");
    }

    /// Flush when the buffer is big; call between messages.
    pub async fn flush_if_big(&mut self) -> Result<(), Closed> {
        if self.buf.len() >= FLUSH_AT {
            self.flush().await
        } else {
            Ok(())
        }
    }

    pub async fn flush(&mut self) -> Result<(), Closed> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let r = self.sink.write(&self.buf).await;
        self.buf.clear();
        r.map_err(|_| Closed)
    }
}
