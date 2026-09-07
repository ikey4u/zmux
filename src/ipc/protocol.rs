use std::io::{self, BufRead, BufReader, Read, Write};

use serde::{Deserialize, Serialize};

// Independent of the application release. Version 1 was never enforced on
// real connections; version 2 requires negotiation before any side effects.
pub const PROTOCOL_VERSION: u16 = 2;
pub const PROTOCOL_MINOR: u16 = 0;
pub const MIN_PEER_MINOR: u16 = 0;
pub const HANDSHAKE_SCHEMA: u16 = 1;
pub const HANDSHAKE_MAGIC: &str = "ZMUX";
pub const HANDSHAKE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolInfo {
    pub product: String,
    pub schema: u16,
    pub major: u16,
    pub minor: u16,
    pub min_peer_minor: u16,
    pub capabilities: Vec<String>,
    pub required_capabilities: Vec<String>,
    pub application_version: String,
}

impl ProtocolInfo {
    pub fn current() -> Self {
        let required = [
            "control-v1",
            "frame-json-v1",
            "session-tree-v1",
            "workspace-home-v1",
        ];
        Self {
            product: "zmux".into(),
            schema: HANDSHAKE_SCHEMA,
            major: PROTOCOL_VERSION,
            minor: PROTOCOL_MINOR,
            min_peer_minor: MIN_PEER_MINOR,
            capabilities: required
                .iter()
                .copied()
                .chain(["ssh-stdio-v1"])
                .map(str::to_string)
                .collect(),
            required_capabilities: required
                .into_iter()
                .map(str::to_string)
                .collect(),
            application_version: crate::platform::ZMUX_VERSION.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NegotiatedProtocol {
    pub major: u16,
    pub minor: u16,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompatibilityError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for CompatibilityError {}

fn incompatible(code: &str, message: impl Into<String>) -> CompatibilityError {
    CompatibilityError {
        code: code.into(),
        message: message.into(),
    }
}

impl From<CompatibilityError> for io::Error {
    fn from(error: CompatibilityError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

pub fn is_compatibility_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<CompatibilityError>())
}

fn validate_info(info: &ProtocolInfo) -> Result<(), CompatibilityError> {
    if info.product != "zmux" || info.schema != HANDSHAKE_SCHEMA {
        return Err(incompatible("unsupported_handshake", "peer does not support the zmux negotiation schema; upgrade both endpoints"));
    }
    let valid_caps = |caps: &[String]| {
        caps.len() <= 64
            && caps.iter().all(|cap| {
                !cap.is_empty()
                    && cap.len() <= 64
                    && cap.bytes().all(|b| {
                        b.is_ascii_lowercase()
                            || b.is_ascii_digit()
                            || b == b'-'
                    })
            })
            && caps.iter().collect::<std::collections::BTreeSet<_>>().len()
                == caps.len()
    };
    if info.min_peer_minor > info.minor
        || info.application_version.len() > 256
        || info.application_version.chars().any(char::is_control)
        || !valid_caps(&info.capabilities)
        || !valid_caps(&info.required_capabilities)
        || !info
            .required_capabilities
            .iter()
            .all(|cap| info.capabilities.contains(cap))
    {
        return Err(incompatible(
            "invalid_protocol_info",
            "peer returned an invalid protocol declaration",
        ));
    }
    Ok(())
}

pub fn negotiate(
    local: &ProtocolInfo,
    peer: &ProtocolInfo,
) -> Result<NegotiatedProtocol, CompatibilityError> {
    validate_info(local)?;
    validate_info(peer)?;
    if local.major != peer.major
        || peer.minor < local.min_peer_minor
        || local.minor < peer.min_peer_minor
    {
        return Err(incompatible("protocol_version_mismatch", format!(
            "local protocol {}.{} (minimum peer {}.{}), peer protocol {}.{} (minimum peer {}.{}; app {}); install compatible zmux versions and restart the incompatible server after saving its sessions",
            local.major, local.minor, local.major, local.min_peer_minor,
            peer.major, peer.minor, peer.major, peer.min_peer_minor, peer.application_version)));
    }
    for (required, available, endpoint) in [
        (&local.required_capabilities, &peer.capabilities, "peer"),
        (&peer.required_capabilities, &local.capabilities, "local"),
    ] {
        if let Some(missing) =
            required.iter().find(|cap| !available.contains(cap))
        {
            return Err(incompatible("missing_capability", format!("{endpoint} lacks required capability {missing}; upgrade the incompatible endpoint")));
        }
    }
    let mut capabilities: Vec<_> = local
        .capabilities
        .iter()
        .filter(|cap| peer.capabilities.contains(cap))
        .cloned()
        .collect();
    capabilities.sort();
    Ok(NegotiatedProtocol {
        major: local.major,
        minor: local.minor.min(peer.minor),
        capabilities,
    })
}

pub fn parse_protocol_info(
    json: &[u8],
) -> Result<ProtocolInfo, CompatibilityError> {
    if json.len() > MAX_HEADER_BYTES {
        return Err(incompatible(
            "invalid_protocol_info",
            "protocol declaration exceeds size limit",
        ));
    }
    let info: ProtocolInfo = serde_json::from_slice(json).map_err(|_| incompatible(
        "invalid_protocol_info", "expected protocol-info JSON; upgrade remote zmux and ensure non-interactive SSH stdout has no shell banners"))?;
    validate_info(&info)?;
    Ok(info)
}

#[derive(Serialize, Deserialize)]
struct Welcome {
    peer: ProtocolInfo,
    negotiated: NegotiatedProtocol,
}

fn send_protocol_message(
    w: &mut (impl Write + ?Sized),
    kind: &str,
    value: &impl Serialize,
) -> io::Result<()> {
    let line = format!(
        "{HANDSHAKE_MAGIC} {kind} {}\n",
        serde_json::to_string(value)?
    );
    if line.len() > MAX_HEADER_BYTES {
        return Err(incompatible(
            "invalid_handshake",
            "handshake exceeds size limit",
        )
        .into());
    }
    w.write_all(line.as_bytes())?;
    w.flush()
}

fn read_handshake_line(r: &mut (impl Read + ?Sized)) -> io::Result<String> {
    let started = std::time::Instant::now();
    let mut line = Vec::new();
    loop {
        if started.elapsed() >= HANDSHAKE_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "protocol handshake deadline exceeded",
            ));
        }
        let mut byte = [0];
        match r.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete protocol handshake",
                ))
            }
            Ok(_) => line.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                continue
            }
            Err(error) => return Err(error),
        }
        if line.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol handshake exceeds size limit",
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(line)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn server_handshake(
    r: &mut impl BufRead,
    w: &mut impl Write,
) -> io::Result<NegotiatedProtocol> {
    let local = ProtocolInfo::current();
    let result = (|| {
        let line = read_handshake_line(r).map_err(|_| {
            incompatible(
                "invalid_handshake",
                "invalid or incomplete client handshake",
            )
        })?;
        let json = line.strip_prefix("ZMUX HELLO ").ok_or_else(|| incompatible(
            "handshake_required", "protocol 2 negotiation required before any command; upgrade the client"))?;
        let peer = parse_protocol_info(json.as_bytes())?;
        negotiate(&local, &peer)
    })();
    match result {
        Ok(negotiated) => {
            send_protocol_message(
                w,
                "WELCOME",
                &Welcome {
                    peer: local,
                    negotiated: negotiated.clone(),
                },
            )?;
            Ok(negotiated)
        }
        Err(error) => {
            let _ = send_protocol_message(w, "REJECT", &error);
            Err(error.into())
        }
    }
}

pub fn client_handshake(
    stream: &mut (impl Read + Write + ?Sized),
) -> io::Result<NegotiatedProtocol> {
    let local = ProtocolInfo::current();
    send_protocol_message(stream, "HELLO", &local)?;
    // Never discard a temporary BufReader's read-ahead: the next bytes belong
    // to the business protocol and must remain available to its own reader.
    let line = read_handshake_line(stream).map_err(|error| match error.kind() {
        io::ErrorKind::UnexpectedEof => io::Error::from(incompatible("handshake_required", "server closed before negotiation; it may be a legacy zmux server. Upgrade/restart it after saving sessions; no commands were sent")),
        io::ErrorKind::InvalidData => io::Error::from(incompatible("invalid_handshake", error.to_string())),
        _ => error,
    })?;
    if let Some(json) = line.strip_prefix("ZMUX REJECT ") {
        let error: CompatibilityError =
            serde_json::from_str(json).map_err(|_| {
                incompatible(
                    "invalid_handshake",
                    "malformed protocol rejection",
                )
            })?;
        return Err(error.into());
    }
    let json = line.strip_prefix("ZMUX WELCOME ").ok_or_else(|| {
        incompatible(
            "handshake_required",
            "server did not negotiate zmux protocol; upgrade the server",
        )
    })?;
    let welcome: Welcome = serde_json::from_str(json).map_err(|_| {
        incompatible("invalid_handshake", "malformed server welcome")
    })?;
    let negotiated = negotiate(&local, &welcome.peer)?;
    if negotiated != welcome.negotiated {
        return Err(incompatible("invalid_handshake", "server selected a protocol/capability set outside the negotiated contract").into());
    }
    Ok(negotiated)
}

pub fn send_ok(
    w: &mut dyn Write,
    session_id: usize,
    version: &str,
) -> io::Result<()> {
    write!(w, "OK {} {}\n", session_id, version)?;
    w.flush()
}

pub fn send_error(w: &mut dyn Write, reason: &str) -> io::Result<()> {
    write!(w, "ERROR {}\n", reason)?;
    w.flush()
}

pub fn send_frame(w: &mut dyn Write, json: &str) -> io::Result<()> {
    write!(w, "FRAME {}\n", json.len())?;
    w.write_all(json.as_bytes())?;
    w.flush()
}

pub fn recv_frame(r: &mut BufReader<impl Read>) -> io::Result<String> {
    let header = read_bounded_line(r)?;
    let header = header.trim();
    let len: usize = header
        .strip_prefix("FRAME ")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "bad frame header")
        })?;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds protocol limit",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn send_cmd(w: &mut dyn Write, json: &str) -> io::Result<()> {
    write!(w, "CMD {}\n", json.len())?;
    w.write_all(json.as_bytes())?;
    w.flush()
}

pub fn recv_line(r: &mut BufReader<impl Read>) -> io::Result<String> {
    let line = read_bounded_line(r)?;
    Ok(line
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string())
}

pub fn recv_resp(r: &mut BufReader<impl Read>) -> io::Result<String> {
    let header = recv_line(r)?;
    if let Some(rest) = header.strip_prefix("RESP ") {
        let len: usize = rest
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if len > MAX_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response exceeds protocol limit",
            ));
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        return String::from_utf8(buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
    }
    Ok(header)
}

fn read_bounded_line(r: &mut impl BufRead) -> io::Result<String> {
    let mut bytes = Vec::new();
    loop {
        let available = r.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len() + take > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol header exceeds limit",
            ));
        }
        bytes.extend_from_slice(&available[..take]);
        r.consume(take);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated protocol header",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn send_resp(w: &mut dyn Write, data: &str) -> io::Result<()> {
    write!(w, "RESP {}\n", data.len())?;
    w.write_all(data.as_bytes())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_depends_on_wire_contract_not_application_release() {
        let local = ProtocolInfo::current();
        let mut peer = local.clone();
        peer.application_version = "99.0.0-other-build".into();
        assert_eq!(negotiate(&local, &peer).unwrap().minor, 0);
        for major in [1, 3] {
            peer.major = major;
            assert_eq!(
                negotiate(&local, &peer).unwrap_err().code,
                "protocol_version_mismatch"
            );
        }
    }

    #[test]
    fn minor_compatibility_is_bidirectional() {
        let mut local = ProtocolInfo::current();
        local.minor = 3;
        local.min_peer_minor = 1;
        let mut peer = ProtocolInfo::current();
        peer.minor = 2;
        assert_eq!(negotiate(&local, &peer).unwrap().minor, 2);
        peer.minor = 0;
        assert_eq!(
            negotiate(&local, &peer).unwrap_err().code,
            "protocol_version_mismatch"
        );
        peer.minor = 4;
        peer.min_peer_minor = 4;
        assert_eq!(
            negotiate(&local, &peer).unwrap_err().code,
            "protocol_version_mismatch"
        );
    }

    #[test]
    fn capabilities_are_checked_in_both_directions() {
        let local = ProtocolInfo::current();
        let mut peer = local.clone();
        peer.capabilities.push("future-optional-v1".into());
        assert!(!negotiate(&local, &peer)
            .unwrap()
            .capabilities
            .contains(&"future-optional-v1".into()));
        peer.required_capabilities.push("future-optional-v1".into());
        assert_eq!(
            negotiate(&local, &peer).unwrap_err().code,
            "missing_capability"
        );
        peer = local.clone();
        peer.capabilities.retain(|cap| cap != "control-v1");
        peer.required_capabilities.retain(|cap| cap != "control-v1");
        assert_eq!(
            negotiate(&local, &peer).unwrap_err().code,
            "missing_capability"
        );
    }

    #[test]
    fn malformed_declarations_fail_closed() {
        for input in [b"{}".as_slice(), b"login banner\n{}", b"not json"] {
            assert!(parse_protocol_info(input).is_err());
        }
        assert!(parse_protocol_info(&vec![b' '; MAX_HEADER_BYTES + 1]).is_err());
        let local = ProtocolInfo::current();
        let mut peer = local.clone();
        peer.schema += 1;
        assert_eq!(
            negotiate(&local, &peer).unwrap_err().code,
            "unsupported_handshake"
        );
        peer = local.clone();
        peer.capabilities.push(peer.capabilities[0].clone());
        assert_eq!(
            negotiate(&local, &peer).unwrap_err().code,
            "invalid_protocol_info"
        );
        peer = local.clone();
        peer.min_peer_minor = peer.minor + 1;
        assert!(negotiate(&local, &peer).is_err());
    }

    struct ScriptStream {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for ScriptStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for ScriptStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn both_handshakes_preserve_following_business_bytes() {
        let local = ProtocolInfo::current();
        let mut bytes = Vec::new();
        send_protocol_message(&mut bytes, "HELLO", &local).unwrap();
        bytes.extend_from_slice(b"ATTACH\n24x80+0+0\n");
        let mut reader = BufReader::new(bytes.as_slice());
        let mut response = Vec::new();
        server_handshake(&mut reader, &mut response).unwrap();
        assert_eq!(recv_line(&mut reader).unwrap(), "ATTACH");
        response.extend_from_slice(b"RESP 2\nOK");
        let mut stream = ScriptStream {
            input: std::io::Cursor::new(response),
            output: vec![],
        };
        client_handshake(&mut stream).unwrap();
        assert_eq!(recv_resp(&mut BufReader::new(stream)).unwrap(), "OK");
    }

    #[test]
    fn legacy_and_truncated_client_requests_are_rejected() {
        for bytes in [
            b"ATTACH\n".as_slice(),
            b"KILL_SERVER\n",
            b"ZMUX 1\n",
            b"ZMUX HELLO {}",
        ] {
            let mut input = BufReader::new(bytes);
            let mut output = Vec::new();
            assert!(is_compatibility_error(
                &server_handshake(&mut input, &mut output).unwrap_err()
            ));
            assert!(output.starts_with(b"ZMUX REJECT "));
        }
    }

    #[test]
    fn client_rejects_missing_or_forged_negotiation() {
        let local = ProtocolInfo::current();
        let mut negotiated = negotiate(&local, &local).unwrap();
        negotiated.minor += 1;
        let mut forged = Vec::new();
        send_protocol_message(
            &mut forged,
            "WELCOME",
            &Welcome {
                peer: local,
                negotiated,
            },
        )
        .unwrap();
        for bytes in [
            vec![],
            b"FRAME 2\n{}".to_vec(),
            b"ZMUX WELCOME {}\n".to_vec(),
            forged,
        ] {
            let mut stream = ScriptStream {
                input: std::io::Cursor::new(bytes),
                output: vec![],
            };
            assert!(is_compatibility_error(
                &client_handshake(&mut stream).unwrap_err()
            ));
            assert!(stream.output.starts_with(b"ZMUX HELLO "));
            assert!(!stream.output.windows(6).any(|w| w == b"ATTACH"));
        }
    }

    #[test]
    fn handshake_size_and_utf8_are_validated_on_both_sides() {
        for bytes in [vec![b'x'; MAX_HEADER_BYTES + 1], vec![0xff, b'\n']] {
            let mut stream = ScriptStream {
                input: std::io::Cursor::new(bytes.clone()),
                output: vec![],
            };
            assert!(is_compatibility_error(
                &client_handshake(&mut stream).unwrap_err()
            ));
            let mut input = BufReader::new(bytes.as_slice());
            let mut output = Vec::new();
            assert!(is_compatibility_error(
                &server_handshake(&mut input, &mut output).unwrap_err()
            ));
        }
    }

    #[test]
    fn server_rejection_retains_machine_readable_code() {
        let mut response = Vec::new();
        send_protocol_message(
            &mut response,
            "REJECT",
            &incompatible("protocol_version_mismatch", "test"),
        )
        .unwrap();
        let mut stream = ScriptStream {
            input: std::io::Cursor::new(response),
            output: vec![],
        };
        let error = client_handshake(&mut stream).unwrap_err();
        assert!(is_compatibility_error(&error));
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<CompatibilityError>()
                .unwrap()
                .code,
            "protocol_version_mismatch"
        );
    }

    #[test]
    fn rejects_oversized_lengths_before_allocating_payloads() {
        let frame = format!("FRAME {}\n", MAX_FRAME_BYTES + 1);
        let mut frame = BufReader::new(frame.as_bytes());
        assert_eq!(
            recv_frame(&mut frame).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let response = format!("RESP {}\n", MAX_RESPONSE_BYTES + 1);
        let mut response = BufReader::new(response.as_bytes());
        assert_eq!(
            recv_resp(&mut response).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_oversized_headers() {
        let input = format!("{}\n", "x".repeat(MAX_HEADER_BYTES + 1));
        let mut input = BufReader::new(input.as_bytes());
        assert_eq!(
            recv_line(&mut input).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
