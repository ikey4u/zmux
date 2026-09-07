use std::{
    ffi::OsStr,
    fs::OpenOptions,
    io::{self, Read, Write},
    os::windows::ffi::OsStrExt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    },
};

pub fn pipe_name(socket_name: &str) -> String {
    format!(r"\\.\pipe\zmux-{}", socket_name)
}

fn to_wstring(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub struct PipeListener {
    name: String,
    first_instance: AtomicBool,
}

pub struct PipeStream {
    inner: std::fs::File,
    read_timeout: Arc<Mutex<Option<Duration>>>,
}

pub struct PipeIncoming<'a> {
    listener: &'a PipeListener,
}

impl PipeListener {
    pub fn incoming(&self) -> PipeIncoming<'_> {
        PipeIncoming { listener: self }
    }

    pub fn accept(&self) -> io::Result<PipeStream> {
        let name_w = to_wstring(&self.name);
        let first = self.first_instance.swap(false, Ordering::AcqRel);
        let handle = unsafe {
            let open_mode = if first {
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                PIPE_ACCESS_DUPLEX
            };
            CreateNamedPipeW(
                name_w.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            if first {
                self.first_instance.store(true, Ordering::Release);
            }
            return Err(io::Error::last_os_error());
        }
        let connected =
            unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if connected == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(535) {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(e);
            }
        }
        use std::os::windows::io::FromRawHandle;
        let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
        Ok(PipeStream {
            inner: file,
            read_timeout: Arc::new(Mutex::new(None)),
        })
    }
}

impl Iterator for PipeIncoming<'_> {
    type Item = io::Result<PipeStream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept())
    }
}

impl PipeStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        self.inner.try_clone().map(|inner| Self {
            inner,
            read_timeout: Arc::clone(&self.read_timeout),
        })
    }

    pub fn set_read_timeout(
        &self,
        timeout: Option<Duration>,
    ) -> io::Result<()> {
        *self
            .read_timeout
            .lock()
            .map_err(|_| io::Error::other("pipe timeout lock poisoned"))? =
            timeout;
        Ok(())
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        if buf.is_empty() {
            return Ok(0);
        }
        let timeout = *self
            .read_timeout
            .lock()
            .map_err(|_| io::Error::other("pipe timeout lock poisoned"))?;
        if let Some(timeout) = timeout {
            let start = Instant::now();
            loop {
                let mut available = 0u32;
                let result = unsafe {
                    PeekNamedPipe(
                        self.inner.as_raw_handle() as _,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                };
                if result == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(109) {
                        return Ok(0);
                    } // ERROR_BROKEN_PIPE
                    return Err(error);
                }
                if available > 0 {
                    let len = buf.len().min(available as usize);
                    return self.inner.read(&mut buf[..len]);
                }
                if start.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "named pipe read timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.inner.read(buf)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn bind_server(socket_name: &str) -> io::Result<PipeListener> {
    Ok(PipeListener {
        name: pipe_name(socket_name),
        first_instance: AtomicBool::new(true),
    })
}

pub fn connect_client(socket_name: &str) -> io::Result<PipeStream> {
    let name = pipe_name(socket_name);
    let file = OpenOptions::new().read(true).write(true).open(&name)?;
    Ok(PipeStream {
        inner: file,
        read_timeout: Arc::new(Mutex::new(None)),
    })
}
