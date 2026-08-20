use std::{
    io,
    process::{Command, Stdio},
    thread,
};

use crate::{
    domain::{
        cloud::CloudClient,
        config::resolve_host,
        hello::{legacy_hint, negotiate, Hello, REQUIRED_CAPS},
        quote::join_quoted,
    },
    types::session::Size,
};

pub fn run_cli(
    host: &str,
    local_socket: &str,
    start_dir: Option<String>,
) -> io::Result<()> {
    if let (Ok(pane), Ok(socket)) = (
        std::env::var("ZMUX_PANE"),
        std::env::var("ZMUX_SOCKET").or_else(|_| {
            if local_socket.is_empty() {
                Err(std::env::VarError::NotPresent)
            } else {
                Ok(local_socket.to_string())
            }
        }),
    ) {
        return domain_attach_wait(&socket, host, &pane);
    }
    if std::env::var_os("ZMUX_PANE").is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "ZMUX_PANE is set but ZMUX_SOCKET is missing",
        ));
    }
    crate::client::ClientApp::new_ssh(host.to_string(), local_socket, start_dir)
        .run()
}

fn domain_attach_wait(socket: &str, host: &str, pane: &str) -> io::Result<()> {
    use std::io::{BufReader, Write};

    use crate::{
        domain::{attach::DomainAttachRequest, ids::new_instance_id},
        ipc::{connect_client, recv_resp},
    };

    let pane_id = pane
        .trim()
        .trim_start_matches('%')
        .parse::<usize>()
        .map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "bad ZMUX_PANE")
        })?;
    let req = DomainAttachRequest {
        request_id: new_instance_id(),
        host: host.to_string(),
        pane_id,
    };
    let json = serde_json::to_string(&req)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let stream = connect_client(socket)?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(60)));
    let mut writer = stream.try_clone()?;
    writer.write_all(format!("DOMAIN_ATTACH {json}\n").as_bytes())?;
    writer.flush()?;
    let mut reader = BufReader::new(stream);
    let resp = recv_resp(&mut reader)?;
    if resp.starts_with("OK") {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, resp))
    }
}

pub fn connect_ssh(alias: &str, size: Size) -> io::Result<CloudClient> {
    let host = resolve_host(alias);
    let probe = ssh_probe(&host)?;
    if probe.legacy_remote || probe.protocol.major != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            legacy_hint(&probe, &host.ssh, &host.socket),
        ));
    }
    if probe.lease_held {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "remote cloud attach is already active on {} socket {}\n\
close the other SSH/cloud client before attaching again",
                host.ssh, host.socket
            ),
        ));
    }
    let local = Hello::offer("client", None, &[]);
    let remote = Hello {
        binary_version: probe
            .server_version
            .clone()
            .unwrap_or_else(|| probe.binary_version.clone()),
        server_instance_id: probe
            .server_instance_id
            .clone()
            .unwrap_or_default(),
        protocol: probe.protocol.clone(),
        capabilities: probe.capabilities.clone(),
        limits: probe.limits.clone(),
        domain: None,
    };
    if let Err(err) = negotiate(&local, &remote) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            legacy_hint(&probe, &host.ssh, &host.socket)
                + "\n"
                + &err.message(),
        ));
    }
    let missing: Vec<_> = REQUIRED_CAPS
        .iter()
        .filter(|cap| !probe.capabilities.iter().any(|c| c == *cap))
        .copied()
        .collect();
    if !missing.is_empty() && probe.server_running {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            legacy_hint(&probe, &host.ssh, &host.socket),
        ));
    }

    let remote_cmd = remote_zmux_command(&[
        &host.remote_zmux,
        "--socket",
        &host.socket,
        "mux",
        "--stdio",
        "--start-if-missing",
    ])?;
    let mut child = ssh_command(&host.ssh)
        .arg(&remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("failed to spawn ssh: {err}"),
            )
        })?;

    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = String::new();
            let mut r = stderr;
            let _ = r.read_to_string(&mut buf);
            if !buf.trim().is_empty() {
                log_ssh(&format!("stderr: {}", buf.trim()));
            }
        });
    }

    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "ssh stdout was not piped")
    })?;
    let stdin = child.stdin.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "ssh stdin was not piped")
    })?;
    thread::spawn(move || {
        let _ = child.wait();
    });

    CloudClient::connect(stdout, stdin, size, &host.alias, &host.socket)
}

pub fn ssh_probe(
    host: &crate::domain::config::SshHost,
) -> io::Result<crate::domain::hello::ProbeReport> {
    let remote_cmd = remote_zmux_command(&[
        &host.remote_zmux,
        "--socket",
        &host.socket,
        "cloud-probe",
        "--json",
    ])?;
    let output = ssh_command(&host.ssh)
        .arg(&remote_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("ssh probe failed: {err}"),
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        if stdout.trim().is_empty() {
            let detail = if stderr.contains("Password")
                || stderr.contains("passphrase")
                || stderr.contains("Verification code")
            {
                "interactive SSH authentication is not supported in v1 (BatchMode=yes)"
                    .to_string()
            } else if stderr.contains("Host key verification failed") {
                "StrictHostKeyChecking rejected the host key".to_string()
            } else if stderr.contains("command not found")
                || stderr.contains("No such file")
            {
                format!(
                    "remote binary '{}' was not found on PATH",
                    host.remote_zmux
                )
            } else {
                stderr
            };
            return Ok(crate::domain::hello::ProbeReport::legacy(None, detail));
        }
    }
    let json = extract_json(&stdout).unwrap_or(stdout.as_str());
    match serde_json::from_str(json) {
        Ok(report) => Ok(report),
        Err(_) => Ok(crate::domain::hello::ProbeReport::legacy(
            None,
            if stderr.is_empty() {
                format!("cloud-probe returned: {}", stdout.trim())
            } else {
                stderr
            },
        )),
    }
}

fn extract_json(stdout: &str) -> Option<&str> {
    let start = stdout.find('{')?;
    let end = stdout.rfind('}')?;
    if end >= start {
        Some(&stdout[start..=end])
    } else {
        None
    }
}

fn remote_zmux_command(args: &[&str]) -> io::Result<String> {
    let command = join_quoted(args)?;
    Ok(format!(
        "export PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:$PATH\"; exec {command}"
    ))
}

fn ssh_command(target: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("--")
        .arg(target);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_options_end_before_target_and_remote_command() {
        let mut cmd = ssh_command("example.test");
        cmd.arg("zmux --socket default cloud-probe --json");
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        let marker = args.iter().position(|arg| arg == "--").unwrap();
        let target = args.iter().position(|arg| arg == "example.test").unwrap();
        let remote = args
            .iter()
            .position(|arg| arg == "zmux --socket default cloud-probe --json")
            .unwrap();
        assert!(marker < target);
        assert!(target < remote);
    }

    #[test]
    fn remote_command_exposes_user_bins_and_orders_global_options() {
        let command = remote_zmux_command(&[
            "zmux",
            "--socket",
            "default",
            "cloud-probe",
            "--json",
        ])
        .unwrap();

        assert_eq!(
            command,
            "export PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:$PATH\"; \
             exec zmux --socket default cloud-probe --json"
        );
    }
}

fn log_ssh(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("zmux_client.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "ssh: {msg}");
    }
}
