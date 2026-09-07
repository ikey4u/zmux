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
const DISCOVERY_MARKER: &str = "ZMUX DISCOVERY 1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteProbe {
    pub executable: String,
}

#[derive(Clone)]
struct SshConnector {
    route: Vec<String>,
    socket_name: String,
    executable: String,
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
        let executable = self.executable.clone();
        let label = route.join("/");
        thread::Builder::new()
            .name(format!("zmux-ssh-{label}"))
            .spawn(move || {
                run_ssh_bridge(bridge, &route, &socket_name, &executable)
            })
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
    executable: &str,
    socket_name: &str,
    size: Size,
) -> io::Result<SocketClient> {
    validate_remote_executable(executable)?;
    SocketClient::connect_with(
        socket_name,
        size,
        Arc::new(SshConnector {
            route: route.to_vec(),
            socket_name: socket_name.to_string(),
            executable: executable.to_string(),
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

pub fn probe(route: &[String]) -> Result<RemoteProbe, RemoteFailure> {
    let payload = discovery_command()
        .map_err(|error| RemoteFailure::permanent(error.to_string()))?;
    let mut command = ssh_command(route, &payload)
        .map_err(|error| RemoteFailure::permanent(error.to_string()))?;
    command.stdin(Stdio::null());
    let (status, stdout, stderr) =
        run_with_timeout(command, Duration::from_secs(15))
            .map_err(|error| RemoteFailure::from_io(&error))?;
    let error = String::from_utf8_lossy(&stderr).trim().to_string();
    if matches!(status.code(), Some(255) | None) {
        return Err(RemoteFailure::transient(if error.is_empty() {
            "SSH transport unavailable".into()
        } else {
            error
        }));
    }

    if !status.success() {
        return Err(match parse_discovery_path(&stdout) {
            Ok((executable, _)) => RemoteFailure::permanent(format!(
                "protocol_info_unavailable: {executable} could not report its protocol contract (exit {}). {error}",
                status.code().unwrap_or(-1)
            )),
            Err(_) if status.code() == Some(127) => RemoteFailure::permanent(
                format!(
                    "zmux_missing: could not resolve an executable zmux on {} from ZMUX_BIN, the SSH command environment, the user's login/interactive shell, or standard user install locations; press R to retry",
                    route.join("/")
                ),
            ),
            Err(discovery_error) => RemoteFailure::permanent(format!(
                "zmux_discovery_failed: {discovery_error}. {error}"
            )),
        });
    }

    let (executable, protocol) =
        parse_discovery_output(&stdout).map_err(|error| {
            RemoteFailure::permanent(format!("zmux_discovery_failed: {error}"))
        })?;
    let peer = crate::ipc::parse_protocol_info(&protocol)
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
    Ok(RemoteProbe { executable })
}

fn discovery_command() -> io::Result<String> {
    // Resolve inside the remote user's configured shell, but keep that shell
    // out of the actual stdio bridge. Interactive startup files are allowed to
    // print banners: their output is captured here and only a verified absolute
    // executable path is emitted through the framed discovery response.
    let resolve_in_shell = crate::domain::quote::posix_quote(
        r#"candidate=$(command -v zmux 2>/dev/null) || exit 127; case "$candidate" in /*) [ -f "$candidate" ] && [ -x "$candidate" ] || exit 127 ;; *) exit 127 ;; esac; printf 'ZMUX_LOGIN_PATH:%s\n' "$candidate""#,
    )?;
    Ok(format!(
        r#"set -f; candidate=''; is_zmux_executable() {{ case "$1" in /*) [ -f "$1" ] && [ -x "$1" ] ;; *) return 1 ;; esac; }}; if [ -n "${{ZMUX_BIN:-}}" ] && is_zmux_executable "$ZMUX_BIN"; then candidate=$ZMUX_BIN; fi; if [ -z "$candidate" ]; then direct=$(command -v zmux 2>/dev/null || :); if is_zmux_executable "$direct"; then candidate=$direct; fi; fi; if [ -z "$candidate" ]; then login_shell=${{SHELL:-$0}}; case "$login_shell" in /*) ;; *) login_shell=$(command -v "$login_shell" 2>/dev/null || :) ;; esac; if [ -n "$login_shell" ] && [ -x "$login_shell" ]; then for shell_flags in -lc -lic; do shell_output=$("$login_shell" "$shell_flags" {resolve_in_shell} </dev/null 2>/dev/null || :); case "$shell_output" in *ZMUX_LOGIN_PATH:*) shell_line=${{shell_output##*ZMUX_LOGIN_PATH:}}; if is_zmux_executable "$shell_line"; then candidate=$shell_line; fi ;; esac; [ -n "$candidate" ] && break; done; fi; fi; if [ -z "$candidate" ] && [ -n "${{HOME:-}}" ]; then for known_path in "$HOME/.local/bin/zmux" "$HOME/bin/zmux" "$HOME/.cargo/bin/zmux" "$HOME/.local/share/mise/shims/zmux" "$HOME/.asdf/shims/zmux" "$HOME/.nix-profile/bin/zmux"; do if is_zmux_executable "$known_path"; then candidate=$known_path; break; fi; done; fi; if [ -z "$candidate" ]; then for known_path in /usr/local/bin/zmux /usr/bin/zmux /opt/homebrew/bin/zmux /nix/var/nix/profiles/default/bin/zmux; do if is_zmux_executable "$known_path"; then candidate=$known_path; break; fi; done; fi; if ! is_zmux_executable "$candidate"; then printf '%s\n' 'zmux executable not found' >&2; exit 127; fi; printf '%s\n%s\n' '{DISCOVERY_MARKER}' "$candidate"; exec "$candidate" protocol-info"#
    ))
}

fn parse_discovery_output(output: &[u8]) -> io::Result<(String, Vec<u8>)> {
    let (executable, protocol_index) = parse_discovery_path(output)?;
    let lines = output
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect::<Vec<_>>();
    let protocol = lines
        .get(protocol_index)
        .filter(|line| !line.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote discovery response omitted protocol metadata",
            )
        })?;
    Ok((executable, protocol.to_vec()))
}

fn parse_discovery_path(output: &[u8]) -> io::Result<(String, usize)> {
    let lines = output
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect::<Vec<_>>();
    let marker = lines
        .iter()
        .rposition(|line| *line == DISCOVERY_MARKER.as_bytes())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote response did not contain a zmux discovery frame",
            )
        })?;
    let executable = lines.get(marker + 1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "remote discovery response omitted the executable path",
        )
    })?;
    let executable = std::str::from_utf8(executable).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "remote zmux path is not valid UTF-8",
        )
    })?;
    validate_remote_executable(executable)?;
    Ok((executable.to_string(), marker + 2))
}

fn validate_remote_executable(executable: &str) -> io::Result<()> {
    if !executable.starts_with('/')
        || executable.len() > 16 * 1024
        || executable.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote zmux path must be an absolute UTF-8 path without control characters",
        ));
    }
    Ok(())
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

fn run_ssh_bridge(
    mut bridge: TcpStream,
    route: &[String],
    socket_name: &str,
    executable: &str,
) {
    let remote_command = match bridge_command(executable, socket_name) {
        Ok(command) => command,
        Err(_) => return,
    };
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

fn bridge_command(executable: &str, socket_name: &str) -> io::Result<String> {
    validate_remote_executable(executable)?;
    let quoted_socket = crate::domain::quote::posix_quote(socket_name)?;
    let quoted_executable = crate::domain::quote::posix_quote(executable)?;
    Ok(format!(
        "exec {quoted_executable} -L {quoted_socket} mux --stdio --start-if-missing"
    ))
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
    fn discovery_frame_tolerates_shell_noise_and_preserves_spaced_path() {
        let info = serde_json::to_string(&crate::ipc::ProtocolInfo::current())
            .unwrap();
        let output = format!(
            "welcome from shell startup\n{DISCOVERY_MARKER}\n/opt/zmux builds/current/zmux\n{info}\ngoodbye\n"
        );
        let (executable, protocol) =
            parse_discovery_output(output.as_bytes()).unwrap();
        assert_eq!(executable, "/opt/zmux builds/current/zmux");
        assert_eq!(
            crate::ipc::parse_protocol_info(&protocol).unwrap(),
            crate::ipc::ProtocolInfo::current()
        );
    }

    #[test]
    fn discovery_frame_rejects_untrusted_executable_paths() {
        for path in ["zmux", "../bin/zmux", "/tmp/zmux\rpath"] {
            let output = format!("{DISCOVERY_MARKER}\n{path}\n{{}}\n");
            assert!(parse_discovery_output(output.as_bytes()).is_err());
        }
    }

    #[test]
    fn discovery_path_is_available_when_protocol_command_fails() {
        let output =
            format!("shell noise\n{DISCOVERY_MARKER}\n/home/dev/bin/zmux\n");
        assert_eq!(
            parse_discovery_path(output.as_bytes()).unwrap().0,
            "/home/dev/bin/zmux"
        );
        assert!(parse_discovery_output(output.as_bytes()).is_err());
    }

    #[test]
    fn discovery_checks_login_and_interactive_shells() {
        let command = discovery_command().unwrap();
        assert!(!command.contains('\n'));
        assert!(command.contains("for shell_flags in -lc -lic"));
        assert!(command.contains("$HOME/.local/bin/zmux"));
        assert!(command.contains("$HOME/.cargo/bin/zmux"));
        assert!(command.contains(DISCOVERY_MARKER));

        // Multi-hop routes quote the entire discovery program into another
        // POSIX command, so the generated payload must remain safe there too.
        assert!(ssh_command(&["jump".into(), "host".into()], &command).is_ok());
    }

    #[test]
    fn bridge_uses_the_verified_absolute_executable() {
        let command =
            bridge_command("/home/dev/tools with spaces/zmux", "work space")
                .unwrap();
        assert_eq!(
            command,
            "exec '/home/dev/tools with spaces/zmux' -L 'work space' mux --stdio --start-if-missing"
        );
        assert!(bridge_command("zmux", "default").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_command_finds_a_login_only_path_with_spaces() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "zmux-remote-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = root.join("login bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("zmux");
        let info = serde_json::to_string(&crate::ipc::ProtocolInfo::current())
            .unwrap();
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' {}\n",
                crate::domain::quote::posix_quote(&info).unwrap()
            ),
        )
        .unwrap();
        let login_shell = root.join("login-shell");
        std::fs::write(
            &login_shell,
            format!(
                "#!/bin/sh\nprintf 'startup banner\\n'\nlast=''\nfor argument do last=$argument; done\nPATH={}:$PATH\nexport PATH\nexec /bin/sh -c \"$last\"\n",
                crate::domain::quote::posix_quote(bin.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        for path in [&executable, &login_shell] {
            std::fs::set_permissions(
                path,
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let output = Command::new("sh")
            .args(["-c", &discovery_command().unwrap()])
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", &login_shell)
            .env("HOME", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (found, protocol) = parse_discovery_output(&output.stdout).unwrap();
        assert_eq!(found, executable.to_str().unwrap());
        assert_eq!(
            crate::ipc::parse_protocol_info(&protocol).unwrap(),
            crate::ipc::ProtocolInfo::current()
        );
        let _ = std::fs::remove_dir_all(root);
    }

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
