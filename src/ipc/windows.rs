use std::{
    ffi::OsStr,
    fs::OpenOptions,
    io,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, GENERIC_READ,
        GENERIC_WRITE, OPEN_EXISTING,
    },
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_ACCESS_DUPLEX,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
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
}

pub struct PipeStream {
    inner: std::fs::File,
}

impl PipeListener {
    pub fn accept(&self) -> io::Result<PipeStream> {
        let name_w = to_wstring(&self.name);
        let handle = unsafe {
            CreateNamedPipeW(
                name_w.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
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
        Ok(PipeStream { inner: file })
    }
}

impl io::Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl io::Write for PipeStream {
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
    })
}

pub fn connect_client(socket_name: &str) -> io::Result<PipeStream> {
    let name = pipe_name(socket_name);
    let file = OpenOptions::new().read(true).write(true).open(&name)?;
    Ok(PipeStream { inner: file })
}
