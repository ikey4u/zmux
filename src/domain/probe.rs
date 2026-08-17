use std::{
    io::{self, BufReader, Write},
    time::Duration,
};

use crate::{
    domain::hello::{Hello, ProbeReport},
    ipc::{connect_client, recv_resp},
    platform::ZMUX_VERSION,
};

pub fn probe_socket(socket_name: &str) -> ProbeReport {
    let stream = match connect_client(socket_name) {
        Ok(stream) => stream,
        Err(_) => return ProbeReport::not_running(),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(err) => {
            return ProbeReport::legacy(
                None,
                format!("could not clone probe socket: {err}"),
            )
        }
    };
    let mut reader = BufReader::new(stream);
    if writer.write_all(b"CLOUD_PROBE\n").is_err() || writer.flush().is_err() {
        return ProbeReport::legacy(
            None,
            "running daemon did not accept CLOUD_PROBE".to_string(),
        );
    }
    match recv_resp(&mut reader) {
        Ok(json) => match serde_json::from_str::<ProbeReport>(&json) {
            Ok(mut report) => {
                report.binary_version = ZMUX_VERSION.to_string();
                report.server_running = true;
                report
            }
            Err(_) => match serde_json::from_str::<Hello>(&json) {
                Ok(hello) => ProbeReport {
                    binary_version: ZMUX_VERSION.to_string(),
                    server_running: true,
                    server_version: Some(hello.binary_version.clone()),
                    server_instance_id: Some(hello.server_instance_id),
                    protocol: hello.protocol,
                    capabilities: hello.capabilities,
                    limits: hello.limits,
                    legacy_remote: false,
                    lease_held: false,
                    error: None,
                },
                Err(_) => ProbeReport::legacy(
                    None,
                    format!("unrecognized CLOUD_PROBE response: {json}"),
                ),
            },
        },
        Err(err) => ProbeReport::legacy(
            None,
            format!("running daemon has no cloud-probe ({err})"),
        ),
    }
}

pub fn print_probe(report: &ProbeReport, json: bool) -> io::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report)
                .unwrap_or_else(|_| "{}".into())
        );
        return Ok(());
    }
    println!("binary_version {}", report.binary_version);
    println!(
        "server_running {}",
        if report.server_running { "yes" } else { "no" }
    );
    if let Some(version) = &report.server_version {
        println!("server_version {version}");
    }
    if let Some(id) = &report.server_instance_id {
        println!("server_instance_id {id}");
    }
    println!(
        "protocol {}.{}.{}",
        report.protocol.major,
        report.protocol.min_minor,
        report.protocol.max_minor
    );
    println!("capabilities {}", report.capabilities.join(","));
    if report.legacy_remote {
        println!("legacy_remote yes");
    }
    if let Some(err) = &report.error {
        println!("error {err}");
    }
    Ok(())
}
