use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use super::{
    socket::{ClientStream, SocketClient, SocketConnector},
    Size,
};

const MAX_SSH_COMMAND_OUTPUT: u64 = 8 * 1024 * 1024;

#[derive(Clone)]
struct SshConnector {
    route: Vec<String>,
    socket_name: String,
}

impl SocketConnector for SshConnector {
    fn connect(&self) -> io::Result<Box<dyn ClientStream>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        let client = TcpStream::connect(address)?;
        let (bridge, _) = listener.accept()?;
        client.set_nodelay(true)?;
        bridge.set_nodelay(true)?;

        let route = self.route.clone();
        let socket_name = self.socket_name.clone();
        let label = route.join("/");
        thread::Builder::new()
            .name(format!("zmux-ssh-{label}"))
            .spawn(move || run_ssh_bridge(bridge, &route, &socket_name))
            .map_err(io::Error::other)?;
        Ok(Box::new(client))
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn initial_read_timeout(&self) -> Duration {
        Duration::from_secs(10)
    }
}

pub fn connect_remote(
    route: &[String],
    socket_name: &str,
    size: Size,
) -> io::Result<SocketClient> {
    SocketClient::connect_with(
        socket_name,
        size,
        Arc::new(SshConnector {
            route: route.to_vec(),
            socket_name: socket_name.to_string(),
        }),
    )
}

#[derive(Clone, Debug)]
pub struct RemoteFailure {
    pub message: String,
    pub retryable: bool,
}

impl RemoteFailure {
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
    pub fn from_io(error: &io::Error) -> Self {
        if crate::ipc::is_compatibility_error(error)
            || error.kind() == io::ErrorKind::InvalidData
        {
            Self::permanent(error.to_string())
        } else {
            Self::transient(error.to_string())
        }
    }
}

pub fn probe(route: &[String]) -> Result<(), RemoteFailure> {
    let mut command = ssh_command(
        route,
        "if ! command -v zmux >/dev/null 2>&1; then exit 127; fi; zmux protocol-info",
    )
    .map_err(|error| RemoteFailure::permanent(error.to_string()))?;
    command.stdin(Stdio::null());
    let (status, stdout, stderr) =
        run_with_timeout(command, Duration::from_secs(15))
            .map_err(|error| RemoteFailure::from_io(&error))?;
    if status.success() {
        let peer = crate::ipc::parse_protocol_info(&stdout)
            .map_err(|error| RemoteFailure::permanent(error.to_string()))?;
        let negotiated =
            crate::ipc::negotiate(&crate::ipc::ProtocolInfo::current(), &peer)
                .map_err(|error| RemoteFailure::permanent(error.to_string()))?;
        if !negotiated
            .capabilities
            .iter()
            .any(|cap| cap == "ssh-stdio-v1")
        {
            return Err(RemoteFailure::permanent("missing_capability: remote lacks ssh-stdio-v1; upgrade remote zmux"));
        }
        Ok(())
    } else {
        let error = String::from_utf8_lossy(&stderr).trim().to_string();
        Err(match status.code() {
            Some(127) => RemoteFailure::permanent(format!("zmux_missing: install zmux on {} and expose it on the non-interactive SSH PATH; press R to retry", route.join("/"))),
            Some(255) | None => RemoteFailure::transient(if error.is_empty() { "SSH transport unavailable".into() } else { error }),
            _ => RemoteFailure::permanent(format!("protocol_info_unavailable: remote zmux must support protocol-info; upgrade it and press R to retry. {error}")),
        })
    }
}

fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> io::Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing child stderr"))?;
    let stdout_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_SSH_COMMAND_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(MAX_SSH_COMMAND_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let mut child_status = None;
    let status = loop {
        if child_status.is_none() {
            child_status = child.try_wait()?;
        }
        // The child can exit while descendants still hold stdout/stderr.
        // Keep the deadline active until both readers have finished as well.
        if stdout_thread.is_finished() && stderr_thread.is_finished() {
            if let Some(status) = child_status {
                break status;
            }
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            {
                use nix::{
                    sys::signal::{killpg, Signal},
                    unistd::Pid,
                };
                let _ =
                    killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            // Dropping the handles detaches the reader threads. Joining here
            // can defeat the timeout when a descendant inherited the pipes.
            drop(stdout_thread);
            drop(stderr_thread);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH probe timed out",
            ));
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout_thread
        .join()
        .map_err(|_| io::Error::other("SSH stdout reader panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| io::Error::other("SSH stderr reader panicked"))??;
    if stdout.len() as u64 > MAX_SSH_COMMAND_OUTPUT
        || stderr.len() as u64 > MAX_SSH_COMMAND_OUTPUT
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SSH command output exceeds limit",
        ));
    }
    Ok((status, stdout, stderr))
}

fn run_ssh_bridge(mut bridge: TcpStream, route: &[String], socket_name: &str) {
    let quoted_socket = match crate::domain::quote::posix_quote(socket_name) {
        Ok(socket) => socket,
        Err(_) => return,
    };
    let remote_command =
        format!("exec zmux -L {quoted_socket} mux --stdio --start-if-missing");
    let mut command = match ssh_command(route, &remote_command) {
        Ok(command) => command,
        Err(_) => return,
    };
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let Some(mut child_stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let Some(mut child_stdout) = child.stdout.take() else {
        drop(child_stdin);
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let mut input = match bridge.try_clone() {
        Ok(input) => input,
        Err(_) => {
            drop(child_stdin);
            drop(child_stdout);
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
    };
    let input_thread = thread::spawn(move || {
        let _ = io::copy(&mut input, &mut child_stdin);
        let _ = child_stdin.flush();
    });
    let _ = io::copy(&mut child_stdout, &mut bridge);
    let _ = bridge.shutdown(Shutdown::Both);
    let _ = input_thread.join();
    let _ = child.kill();
    let _ = child.wait();
}

fn ssh_command(route: &[String], payload: &str) -> io::Result<Command> {
    let Some(first) = route.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty SSH route",
        ));
    };
    let mut remote_command = payload.to_string();
    for hop in route.iter().skip(1).rev() {
        remote_command = format!(
            "exec ssh -T -o BatchMode=yes -o ConnectTimeout=5 {} {}",
            crate::domain::quote::posix_quote(hop)?,
            crate::domain::quote::posix_quote(&remote_command)?,
        );
    }
    let mut command = Command::new("ssh");
    command.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
    #[cfg(unix)]
    command.args([
        "-o",
        "ControlMaster=auto",
        "-o",
        "ControlPersist=60",
        "-o",
        "ControlPath=/tmp/zmux-ssh-%C",
    ]);
    command.arg(first).arg(&remote_command);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_route_builds_quoted_ssh_chain() {
        let command =
            ssh_command(&["jump".into(), "prod".into()], "zmux mux --help")
                .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "jump"));
        let remote = args.last().unwrap();
        assert!(remote.contains("exec ssh -T"));
        assert!(remote.contains("prod"));
        assert!(remote.contains("'zmux mux --help'"));
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_kills_descendants_without_waiting_for_their_pipes() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let started = Instant::now();
        let error = run_with_timeout(command, Duration::from_millis(50))
            .expect_err("command should time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_still_applies_after_parent_exits() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5 & exit 0"]);
        let started = Instant::now();
        assert_eq!(
            run_with_timeout(command, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
