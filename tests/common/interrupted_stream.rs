//! TCP fixture adapter: retry only EINTR with the existing syscall budget.
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    ops::Deref,
    time::{Duration, Instant},
};

pub struct InterruptedTcpStream(TcpStream);
impl From<TcpStream> for InterruptedTcpStream {
    fn from(stream: TcpStream) -> Self {
        Self(stream)
    }
}
impl Deref for InterruptedTcpStream {
    type Target = TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
fn retry(
    timeout: Option<Duration>,
    mut operation: impl FnMut(Option<Duration>) -> io::Result<usize>,
) -> io::Result<usize> {
    let deadline = Instant::now() + timeout.unwrap_or(Duration::from_secs(5));
    let mut remaining = None;
    loop {
        match operation(remaining) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                remaining = Some(left);
            }
            result => return result,
        }
    }
}
impl Read for InterruptedTcpStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let timeout = self.0.read_timeout()?;
        let result = retry(timeout, |remaining| {
            if let Some(left) = remaining {
                self.0.set_read_timeout(Some(left))?;
            }
            self.0.read(bytes)
        });
        self.0.set_read_timeout(timeout)?;
        result
    }
}
impl Write for InterruptedTcpStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let timeout = self.0.write_timeout()?;
        let result = retry(timeout, |remaining| {
            if let Some(left) = remaining {
                self.0.set_write_timeout(Some(left))?;
            }
            self.0.write(bytes)
        });
        self.0.set_write_timeout(timeout)?;
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
