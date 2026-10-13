use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream as StreamImpl;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::net::UnixListener as ListenerImpl;

#[cfg(unix)]
use std::os::unix::net::SocketAddr;

/// This wrapper makes UnixStream IoSafe on all platforms.
/// This isn't strictly needed on unix, because async-io
/// includes an impl for the std UnixStream, but on Windows
/// the uds_windows crate doesn't have an impl.
/// Here we define it for all platforms in the interest of
/// minimizing platform differences.
#[derive(Debug)]
pub struct UnixStream(StreamImpl);

#[cfg(unix)]
impl AsFd for UnixStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
#[cfg(unix)]
impl IntoRawFd for UnixStream {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}
#[cfg(unix)]
impl FromRawFd for UnixStream {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixStream {
        UnixStream(StreamImpl::from_raw_fd(fd))
    }
}
#[cfg(unix)]
impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}


impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.0.read(buf)
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.0.flush()
    }
}

unsafe impl async_io::IoSafe for UnixStream {}

impl UnixStream {
    pub fn connect<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self(StreamImpl::connect(path)?))
    }
}

impl std::ops::Deref for UnixStream {
    type Target = StreamImpl;
    fn deref(&self) -> &StreamImpl {
        &self.0
    }
}

impl std::ops::DerefMut for UnixStream {
    fn deref_mut(&mut self) -> &mut StreamImpl {
        &mut self.0
    }
}

pub struct UnixListener(ListenerImpl);

impl UnixListener {
    pub fn bind<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self(ListenerImpl::bind(path)?))
    }

    pub fn accept(&self) -> std::io::Result<(UnixStream, SocketAddr)> {
        let (stream, addr) = self.0.accept()?;
        Ok((UnixStream(stream), addr))
    }

    pub fn incoming(&self) -> impl Iterator<Item = std::io::Result<UnixStream>> + '_ {
        self.0.incoming().map(|r| r.map(UnixStream))
    }
}

impl std::ops::Deref for UnixListener {
    type Target = ListenerImpl;
    fn deref(&self) -> &ListenerImpl {
        &self.0
    }
}

impl std::ops::DerefMut for UnixListener {
    fn deref_mut(&mut self) -> &mut ListenerImpl {
        &mut self.0
    }
}
