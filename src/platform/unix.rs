use std::{io, path::PathBuf};

pub fn spawn_server_background(
    exe: &PathBuf,
    socket_name: &str,
    session_name: &str,
    start_dir: Option<&str>,
) -> io::Result<()> {
    let exe = exe.to_path_buf();
    let socket_name = socket_name.to_string();
    let session_name = session_name.to_string();
    let start_dir = start_dir.map(|dir| dir.to_string());

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid > 0 {
            let mut status = 0i32;
            libc::waitpid(pid, &mut status, 0);
            return Ok(());
        }

        libc::setsid();

        let pid2 = libc::fork();
        if pid2 < 0 {
            libc::exit(1);
        }
        if pid2 > 0 {
            libc::exit(0);
        }

        let devnull = libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }

        let exe_cstr = std::ffi::CString::new(exe.to_string_lossy().as_bytes())
            .unwrap_or_else(|_| libc::exit(1) as std::ffi::CString);
        let l_arg = std::ffi::CString::new("-L").unwrap();
        let sock_arg = std::ffi::CString::new(socket_name.as_bytes()).unwrap();
        let s_arg = std::ffi::CString::new("-s").unwrap();
        let sess_arg = std::ffi::CString::new(session_name.as_bytes()).unwrap();
        let srv_arg = std::ffi::CString::new("server").unwrap();
        let dir_flag = std::ffi::CString::new("--directory").unwrap();
        let dir_arg = start_dir.as_ref().map(|dir| {
            std::ffi::CString::new(dir.as_bytes())
                .unwrap_or_else(|_| libc::exit(1) as std::ffi::CString)
        });

        let mut argv: Vec<*const libc::c_char> = vec![
            exe_cstr.as_ptr(),
            l_arg.as_ptr(),
            sock_arg.as_ptr(),
            s_arg.as_ptr(),
            sess_arg.as_ptr(),
        ];
        if let Some(dir_arg) = dir_arg.as_ref() {
            argv.push(dir_flag.as_ptr());
            argv.push(dir_arg.as_ptr());
        }
        argv.push(srv_arg.as_ptr());
        argv.push(std::ptr::null());

        libc::execv(exe_cstr.as_ptr(), argv.as_ptr());
        libc::exit(1);
    }
}

pub fn setup_signals() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

pub fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGHUP);
    }
}
