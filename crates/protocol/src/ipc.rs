use std::{io, path::Path};

#[cfg(unix)]
mod sys {
    use std::{
        io::{self, Read, Write},
        os::unix::net::{UnixListener, UnixStream},
        path::Path,
        time::Duration,
    };

    pub struct LocalStream {
        inner: UnixStream,
    }

    impl LocalStream {
        pub fn connect(path: &Path) -> io::Result<Self> {
            Ok(Self {
                inner: UnixStream::connect(path)?,
            })
        }

        pub fn peer_is_self(&self) -> bool {
            #[cfg(target_os = "macos")]
            {
                use std::os::fd::AsRawFd;
                let mut uid: libc::uid_t = 0;
                let mut gid: libc::gid_t = 0;
                return unsafe {
                    libc::getpeereid(self.inner.as_raw_fd(), &mut uid, &mut gid) == 0
                        && uid == libc::geteuid()
                };
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = self;
                false
            }
        }

        pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.inner.set_read_timeout(timeout)
        }

        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.inner.set_write_timeout(timeout)
        }

        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.inner.set_nonblocking(nonblocking)
        }
    }

    impl Read for LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    pub struct LocalListener {
        inner: UnixListener,
    }

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            let listener = UnixListener::bind(path)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self { inner: listener })
        }

        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.inner.set_nonblocking(nonblocking)
        }

        pub fn accept(&self) -> io::Result<LocalStream> {
            self.inner.accept().map(|(inner, _)| LocalStream { inner })
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::{
        cell::Cell,
        io::{self, Read, Write},
        os::windows::io::OwnedHandle,
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
        time::Duration,
    };

    use super::super::winutil::{self, PendingConnect};

    pub struct LocalStream {
        handle: OwnedHandle,
        server_end: bool,
        read_timeout: Cell<Option<Duration>>,
        write_timeout: Cell<Option<Duration>>,
    }

    impl LocalStream {
        pub fn connect(path: &Path) -> io::Result<Self> {
            Ok(Self {
                handle: winutil::open_pipe_client(&winutil::pipe_name_for(path))?,
                server_end: false,
                read_timeout: Cell::new(None),
                write_timeout: Cell::new(None),
            })
        }

        pub fn peer_is_self(&self) -> bool {
            winutil::pipe_peer_is_self(&self.handle, self.server_end)
        }

        pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.read_timeout.set(timeout);
            Ok(())
        }

        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.write_timeout.set(timeout);
            Ok(())
        }

        pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            winutil::overlapped_transfer(&self.handle, buf, false, self.read_timeout.get())
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let mut owned = buf.to_vec();
            winutil::overlapped_transfer(&self.handle, &mut owned, true, self.write_timeout.get())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct LocalListener {
        name: String,
        nonblocking: AtomicBool,
        /// 항상 연결을 기다리는 인스턴스 하나를 둔다. 없으면 그 사이 접속한 클라이언트가
        /// "파이프 없음"으로 바로 실패한다(대기할 수 있는 PIPE_BUSY와 다르다).
        pending: Mutex<PendingConnect>,
    }

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            let name = winutil::pipe_name_for(path);
            // 첫 인스턴스 독점(FILE_FLAG_FIRST_PIPE_INSTANCE): 같은 이름을 다른 프로세스가 먼저 만들었으면 실패한다.
            let pending = PendingConnect::begin(&name, true)?;
            Ok(Self {
                name,
                nonblocking: AtomicBool::new(false),
                pending: Mutex::new(pending),
            })
        }

        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.nonblocking.store(nonblocking, Ordering::Relaxed);
            Ok(())
        }

        pub fn accept(&self) -> io::Result<LocalStream> {
            let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());
            let timeout = if self.nonblocking.load(Ordering::Relaxed) {
                0
            } else {
                u32::MAX
            };
            if pending.poll(timeout)?.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "named pipe 연결이 아직 없습니다",
                ));
            }
            // 연결된 인스턴스를 내주기 전에 다음 대기 인스턴스를 먼저 만든다.
            let next = PendingConnect::begin(&self.name, false)?;
            let connected = std::mem::replace(&mut *pending, next);
            Ok(LocalStream {
                handle: connected.into_handle(),
                server_end: true,
                read_timeout: Cell::new(None),
                write_timeout: Cell::new(None),
            })
        }
    }
}

pub use sys::{LocalListener, LocalStream};

impl LocalListener {
    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }
}

pub struct Incoming<'a> {
    listener: &'a LocalListener,
}

impl Iterator for Incoming<'_> {
    type Item = io::Result<LocalStream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept())
    }
}

pub fn connect(path: &Path) -> io::Result<LocalStream> {
    LocalStream::connect(path)
}

pub fn peer_is_self(stream: &LocalStream) -> bool {
    stream.peer_is_self()
}
