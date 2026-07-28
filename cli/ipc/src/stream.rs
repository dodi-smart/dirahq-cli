//! [`Stream`]: the connected duplex byte-stream returned by [`crate::connect`] and
//! [`crate::Listener::accept`] — a Unix domain socket on unix, one end of a named pipe
//! on windows.
//!
//! `AsyncRead`/`AsyncWrite` are implemented by delegating to whichever inner tokio
//! type is active for a given platform. Every variant is already `Unpin` (tokio's own
//! socket and named-pipe types are), so the delegation is a plain
//! `Pin::new(&mut inner)` per poll fn — no `pin-project` dependency needed.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A connected IPC byte-stream, erased across the unix/windows transport split.
#[derive(Debug)]
pub enum Stream {
    /// One end of a Unix domain socket connection.
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    /// The client end of a named pipe connection, from [`crate::connect`].
    #[cfg(windows)]
    PipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
    /// The server end of a named pipe connection, from [`crate::Listener::accept`].
    #[cfg(windows)]
    PipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
}

/// Forward one poll method to whichever inner tokio type this `Stream` wraps.
///
/// All four poll fns below dispatch identically and differ only in name and
/// arguments, so the cfg-gated arm list lives here once instead of four times over
/// — which also means a fifth transport is one new arm, not five.
macro_rules! poll_inner {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self.get_mut() {
            #[cfg(unix)]
            Stream::Unix(s) => Pin::new(s).$method($($arg),*),
            #[cfg(windows)]
            Stream::PipeClient(s) => Pin::new(s).$method($($arg),*),
            #[cfg(windows)]
            Stream::PipeServer(s) => Pin::new(s).$method($($arg),*),
        }
    };
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        poll_inner!(self, poll_read, cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_inner!(self, poll_write, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_inner!(self, poll_flush, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_inner!(self, poll_shutdown, cx)
    }
}
