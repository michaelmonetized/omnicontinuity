use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

enum Descriptor {
    Poll(AsyncFd<File>),
    Regular(File),
}

pub(crate) struct Stdio {
    descriptor: Descriptor,
    flags: OFlags,
}

impl Stdio {
    /// Borrow standard IO without uninterruptible reader threads.
    ///
    /// Takes a descriptor and returns a duplicated nonblocking adapter. Socket,
    /// pipe, and terminal readiness is cancellable; regular files use direct IO.
    /// Original descriptor flags are restored when the adapter is dropped.
    pub fn new(fd: impl AsFd) -> io::Result<Self> {
        let file = File::from(rustix::io::dup(fd)?);
        let flags = fcntl_getfl(&file)?;
        if file.metadata()?.is_file() {
            return Ok(Self {
                descriptor: Descriptor::Regular(file),
                flags,
            });
        }
        let descriptor = file.try_clone()?;
        fcntl_setfl(&file, flags | OFlags::NONBLOCK)?;
        match register(descriptor) {
            Ok(descriptor) => Ok(Self {
                descriptor: Descriptor::Poll(descriptor),
                flags,
            }),
            Err(error) => {
                let _ = fcntl_setfl(&file, flags);
                Err(error)
            }
        }
    }
}

/// Register an owned file for readiness.
///
/// Takes ownership of a valid File and returns its reactor registration. File
/// ownership keeps the descriptor open and unchanged until AsyncFd drops it.
fn register(file: File) -> io::Result<AsyncFd<File>> {
    unsafe { AsyncFd::register(file) }.map_err(io::Error::from)
}

impl Drop for Stdio {
    fn drop(&mut self) {
        match &self.descriptor {
            Descriptor::Poll(fd) => {
                let _ = fcntl_setfl(fd.get_ref(), self.flags);
            }
            Descriptor::Regular(_) => {}
        }
    }
}

impl AsyncRead for Stdio {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let bytes = buffer.initialize_unfilled();
        let read = match &self.get_mut().descriptor {
            Descriptor::Regular(file) => rustix::io::read(file, bytes).map_err(io::Error::from),
            Descriptor::Poll(fd) => loop {
                let mut readiness = ready!(fd.poll_read_ready(cx))?;
                match readiness.try_io(|fd| {
                    rustix::io::read(fd.get_ref(), &mut *bytes).map_err(io::Error::from)
                }) {
                    Ok(result) => break result,
                    Err(_) => continue,
                }
            },
        };
        match read {
            Ok(count) => {
                buffer.advance(count);
                Poll::Ready(Ok(()))
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

impl AsyncWrite for Stdio {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &self.get_mut().descriptor {
            Descriptor::Regular(file) => {
                Poll::Ready(rustix::io::write(file, bytes).map_err(io::Error::from))
            }
            Descriptor::Poll(fd) => loop {
                let mut readiness = ready!(fd.poll_write_ready(cx))?;
                match readiness
                    .try_io(|fd| rustix::io::write(fd.get_ref(), bytes).map_err(io::Error::from))
                {
                    Ok(result) => return Poll::Ready(result),
                    Err(_) => continue,
                }
            },
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
