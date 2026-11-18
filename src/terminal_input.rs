//! Nonblocking terminal input keeps the WebSocket owner free to handle PING and deadlines.
use colabcli_rust::{
    Error, Result,
    jupyter::{InputHandler, InputRequest},
};

#[cfg(unix)]
mod native {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    };
    use zeroize::Zeroize;

    pub struct TerminalInput {
        file: File,
        saved: libc::termios,
        active: Option<String>,
        password: bool,
        buffer: Vec<u8>,
    }
    impl TerminalInput {
        pub fn open() -> Result<Self> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                .open("/dev/tty")
                .map_err(|_| Error::KernelProtocol("交互输入需要可用终端"))?;
            Self::from_file(file)
        }
        fn from_file(file: File) -> Result<Self> {
            let mut saved = std::mem::MaybeUninit::uninit();
            // SAFETY: tcgetattr initializes the supplied termios on success; file remains owned.
            if unsafe { libc::tcgetattr(file.as_raw_fd(), saved.as_mut_ptr()) } != 0 {
                return Err(Error::KernelProtocol("无法读取终端设置"));
            }
            // SAFETY: checked successful tcgetattr above.
            let saved = unsafe { saved.assume_init() };
            Ok(Self {
                file,
                saved,
                active: None,
                password: false,
                buffer: Vec::new(),
            })
        }
        fn restore(&mut self) -> Result<()> {
            // SAFETY: valid owned terminal descriptor and initialized termios.
            if unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.saved) } != 0 {
                return Err(Error::KernelProtocol("无法恢复终端设置"));
            }
            Ok(())
        }
    }
    impl InputHandler for TerminalInput {
        fn poll(&mut self, request: &InputRequest) -> Result<Option<String>> {
            if self.active.as_deref() != Some(&request.id) {
                self.restore()?;
                self.active = Some(request.id.clone());
                self.password = request.password;
                if request.password {
                    let mut settings = self.saved;
                    settings.c_lflag &= !(libc::ECHO | libc::ECHONL);
                    // SAFETY: valid owned descriptor and initialized modified termios.
                    if unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &settings) }
                        != 0
                    {
                        return Err(Error::KernelProtocol("无法关闭密码输入回显"));
                    }
                }
                let mut stderr = std::io::stderr().lock();
                write!(stderr, "{}", request.prompt)
                    .and_then(|_| stderr.flush())
                    .map_err(|_| Error::KernelProtocol("无法显示输入提示"))?;
            }
            let mut chunk = [0u8; 4096];
            match self.file.read(&mut chunk) {
                Ok(0) => return Err(Error::KernelProtocol("交互输入已结束")),
                Ok(size) => self.buffer.extend_from_slice(&chunk[..size]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(Error::KernelProtocol("终端读取失败")),
            }
            chunk.zeroize();
            if self.buffer.len() > 1024 * 1024 {
                return Err(Error::KernelProtocol("交互输入超过 1 MiB"));
            }
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let value = std::str::from_utf8(&line)
                    .map(str::to_owned)
                    .map_err(|_| Error::KernelProtocol("交互输入必须为 UTF-8"));
                line.zeroize();
                self.restore()?;
                if self.password {
                    eprintln!();
                }
                self.active = None;
                return value.map(Some);
            }
            Ok(None)
        }
    }
    impl Drop for TerminalInput {
        fn drop(&mut self) {
            let _ = self.restore();
            self.buffer.zeroize();
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::FromRawFd;
        fn terminal() -> (File, TerminalInput) {
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: output descriptors are valid pointers, optional settings are null.
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            // SAFETY: successful openpty returned distinct owned descriptors.
            let master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
                // SAFETY: both descriptors remain owned and open.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                assert!(flags >= 0);
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
                    0
                );
            }
            (master, TerminalInput::from_file(slave).unwrap())
        }
        fn echo(fd: i32) -> bool {
            let mut attrs = std::mem::MaybeUninit::uninit();
            // SAFETY: valid terminal descriptor and output storage.
            assert_eq!(unsafe { libc::tcgetattr(fd, attrs.as_mut_ptr()) }, 0);
            unsafe { attrs.assume_init().c_lflag & libc::ECHO != 0 }
        }
        #[test]
        fn password_poll_never_echoes_and_restores_terminal_after_reply() {
            let (mut master, mut input) = terminal();
            let request = InputRequest {
                id: "password-1".into(),
                prompt: "".into(),
                password: true,
            };
            assert!(input.poll(&request).unwrap().is_none());
            assert!(!echo(master.as_raw_fd()));
            master.write_all("密码-test\n".as_bytes()).unwrap();
            let began = std::time::Instant::now();
            let value = loop {
                if let Some(value) = input.poll(&request).unwrap() {
                    break value;
                }
                assert!(began.elapsed() < std::time::Duration::from_secs(1));
                std::thread::sleep(std::time::Duration::from_millis(5));
            };
            assert_eq!(value, "密码-test");
            let mut bytes = [0; 128];
            assert_eq!(
                master.read(&mut bytes).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            assert!(echo(master.as_raw_fd()));
        }
        #[test]
        fn abandoning_pending_password_restores_echo_and_normal_input_accepts_utf8() {
            let (mut master, mut input) = terminal();
            let request = InputRequest {
                id: "plain-1".into(),
                prompt: "".into(),
                password: false,
            };
            assert!(input.poll(&request).unwrap().is_none());
            master.write_all("你好\n".as_bytes()).unwrap();
            let began = std::time::Instant::now();
            loop {
                if let Some(value) = input.poll(&request).unwrap() {
                    assert_eq!(value, "你好");
                    break;
                }
                assert!(began.elapsed() < std::time::Duration::from_secs(1));
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let request = InputRequest {
                id: "password-2".into(),
                prompt: "".into(),
                password: true,
            };
            assert!(input.poll(&request).unwrap().is_none());
            assert!(!echo(master.as_raw_fd()));
            drop(input);
            assert!(echo(master.as_raw_fd()));
        }
    }
}
#[cfg(unix)]
pub use native::TerminalInput;

#[cfg(not(unix))]
pub struct TerminalInput;
#[cfg(not(unix))]
impl TerminalInput {
    pub fn open() -> Result<Self> {
        Err(Error::KernelProtocol("此平台尚未实现交互终端输入"))
    }
}
#[cfg(not(unix))]
impl InputHandler for TerminalInput {
    fn poll(&mut self, _: &InputRequest) -> Result<Option<String>> {
        unreachable!()
    }
}

/// Already-authorized automation may run without a TTY; never block if interaction is requested.
pub struct UnavailableInput;
impl InputHandler for UnavailableInput {
    fn poll(&mut self, _: &InputRequest) -> Result<Option<String>> {
        Err(Error::InteractionRequired)
    }
}
