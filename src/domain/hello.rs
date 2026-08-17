use serde::{Deserialize, Serialize};

use crate::{
    ipc::v2::{
        MAX_BLOB_CHUNK, MAX_ENVELOPE_PAYLOAD, PROTOCOL_MAJOR,
        PROTOCOL_MAX_MINOR, PROTOCOL_MIN_MINOR,
    },
    platform::ZMUX_VERSION,
};

pub const CAP_DOMAIN_FRAME_V1: &str = "domain-frame-v1";
pub const CAP_TARGETED_PANE_V1: &str = "targeted-pane-v1";
pub const CAP_CLIENT_LEASE_V1: &str = "client-lease-v1";
pub const CAP_BLOB_V1: &str = "blob-v1";
pub const CAP_CLIPBOARD_IMAGE_V1: &str = "clipboard-image-v1";

pub const REQUIRED_CAPS: [&str; 3] = [
    CAP_DOMAIN_FRAME_V1,
    CAP_TARGETED_PANE_V1,
    CAP_CLIENT_LEASE_V1,
];

pub const OPTIONAL_CAPS: [&str; 2] = [CAP_BLOB_V1, CAP_CLIPBOARD_IMAGE_V1];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolRange {
    pub major: u8,
    pub min_minor: u8,
    pub max_minor: u8,
}

impl ProtocolRange {
    pub fn current() -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            min_minor: PROTOCOL_MIN_MINOR,
            max_minor: PROTOCOL_MAX_MINOR,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limits {
    pub max_frame: u32,
    pub max_blob_chunk: u32,
}

impl Limits {
    pub fn current() -> Self {
        Self {
            max_frame: MAX_ENVELOPE_PAYLOAD,
            max_blob_chunk: MAX_BLOB_CHUNK,
        }
    }

    pub fn intersect(&self, other: &Self) -> Self {
        Self {
            max_frame: self.max_frame.min(other.max_frame),
            max_blob_chunk: self.max_blob_chunk.min(other.max_blob_chunk),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub binary_version: String,
    pub server_instance_id: String,
    pub protocol: ProtocolRange,
    pub capabilities: Vec<String>,
    pub limits: Limits,
    #[serde(default)]
    pub domain: Option<HelloDomain>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloDomain {
    pub transport: String,
    pub host_alias: String,
    pub remote_socket: String,
}

impl Hello {
    pub fn offer(
        server_instance_id: impl Into<String>,
        domain: Option<HelloDomain>,
        extra_caps: &[&str],
    ) -> Self {
        let mut capabilities = REQUIRED_CAPS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        for cap in extra_caps {
            if !capabilities.iter().any(|c| c == cap) {
                capabilities.push((*cap).to_string());
            }
        }
        Self {
            binary_version: ZMUX_VERSION.to_string(),
            server_instance_id: server_instance_id.into(),
            protocol: ProtocolRange::current(),
            capabilities,
            limits: Limits::current(),
            domain,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProbeReport {
    pub binary_version: String,
    pub server_running: bool,
    #[serde(default)]
    pub server_version: Option<String>,
    #[serde(default)]
    pub server_instance_id: Option<String>,
    pub protocol: ProtocolRange,
    pub capabilities: Vec<String>,
    pub limits: Limits,
    #[serde(default)]
    pub legacy_remote: bool,
    #[serde(default)]
    pub lease_held: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl ProbeReport {
    pub fn not_running() -> Self {
        Self {
            binary_version: ZMUX_VERSION.to_string(),
            server_running: false,
            server_version: None,
            server_instance_id: None,
            protocol: ProtocolRange::current(),
            capabilities: Hello::offer("none", None, &[]).capabilities,
            limits: Limits::current(),
            legacy_remote: false,
            lease_held: false,
            error: None,
        }
    }

    pub fn legacy(server_version: Option<String>, detail: String) -> Self {
        Self {
            binary_version: ZMUX_VERSION.to_string(),
            server_running: true,
            server_version,
            server_instance_id: None,
            protocol: ProtocolRange {
                major: 1,
                min_minor: 0,
                max_minor: 0,
            },
            capabilities: Vec::new(),
            limits: Limits::current(),
            legacy_remote: true,
            lease_held: false,
            error: Some(detail),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub minor: u8,
    pub capabilities: Vec<String>,
    pub limits: Limits,
    pub peer: Hello,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Incompatible {
    pub reason: String,
    pub message: String,
    pub hint: String,
    #[serde(default)]
    pub local: Option<Hello>,
    #[serde(default)]
    pub remote: Option<Hello>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiateError {
    MajorMismatch {
        local: u8,
        remote: u8,
    },
    MinorMismatch {
        local: ProtocolRange,
        remote: ProtocolRange,
    },
    MissingRequired(Vec<String>),
}

impl NegotiateError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::MajorMismatch { .. } => "protocol_major",
            Self::MinorMismatch { .. } => "protocol_minor",
            Self::MissingRequired(_) => "missing_capability",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::MajorMismatch { local, remote } => {
                format!("protocol major {remote} != {local}")
            }
            Self::MinorMismatch { local, remote } => format!(
                "no overlap between minor {}-{} and {}-{}",
                local.min_minor,
                local.max_minor,
                remote.min_minor,
                remote.max_minor
            ),
            Self::MissingRequired(caps) => {
                format!("missing required capabilities: {}", caps.join(", "))
            }
        }
    }
}

pub fn negotiate(
    local: &Hello,
    remote: &Hello,
) -> Result<Negotiated, NegotiateError> {
    if local.protocol.major != remote.protocol.major {
        return Err(NegotiateError::MajorMismatch {
            local: local.protocol.major,
            remote: remote.protocol.major,
        });
    }
    let min_minor = local.protocol.min_minor.max(remote.protocol.min_minor);
    let max_minor = local.protocol.max_minor.min(remote.protocol.max_minor);
    if min_minor > max_minor {
        return Err(NegotiateError::MinorMismatch {
            local: local.protocol.clone(),
            remote: remote.protocol.clone(),
        });
    }
    let missing = REQUIRED_CAPS
        .iter()
        .filter(|cap| {
            !remote.capabilities.iter().any(|c| c == *cap)
                || !local.capabilities.iter().any(|c| c == *cap)
        })
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(NegotiateError::MissingRequired(missing));
    }
    let capabilities = local
        .capabilities
        .iter()
        .filter(|cap| remote.capabilities.iter().any(|c| c == *cap))
        .cloned()
        .collect();
    Ok(Negotiated {
        minor: max_minor,
        capabilities,
        limits: local.limits.intersect(&remote.limits),
        peer: remote.clone(),
    })
}

pub fn incompatible_from(
    err: &NegotiateError,
    local: &Hello,
    remote: &Hello,
) -> Incompatible {
    Incompatible {
        reason: err.reason().to_string(),
        message: err.message(),
        hint: upgrade_hint(remote),
        local: Some(local.clone()),
        remote: Some(remote.clone()),
    }
}

pub fn upgrade_hint(remote: &Hello) -> String {
    format!(
        "remote zmux {} does not support cloud attach\n\
required: protocol {} + {} + {} + {}\n\
running daemon: {} on socket {}\n\
upgrade the remote binary and restart that daemon, or use: ssh {} -t zmux a",
        remote.binary_version,
        PROTOCOL_MAJOR,
        CAP_DOMAIN_FRAME_V1,
        CAP_TARGETED_PANE_V1,
        CAP_CLIENT_LEASE_V1,
        remote.binary_version,
        remote
            .domain
            .as_ref()
            .map(|d| d.remote_socket.as_str())
            .unwrap_or("default"),
        remote
            .domain
            .as_ref()
            .map(|d| d.host_alias.as_str())
            .unwrap_or("host"),
    )
}

pub fn legacy_hint(report: &ProbeReport, host: &str, socket: &str) -> String {
    let version = report.server_version.as_deref().unwrap_or("unknown");
    format!(
        "remote zmux {version} does not support cloud attach\n\
required: protocol {PROTOCOL_MAJOR} + {CAP_DOMAIN_FRAME_V1} + {CAP_TARGETED_PANE_V1} + {CAP_CLIENT_LEASE_V1}\n\
running daemon: {version} on socket {socket}\n\
upgrade the remote binary and restart that daemon, or use: ssh {host} -t zmux a"
    )
}

pub fn has_cap(caps: &[String], name: &str) -> bool {
    caps.iter().any(|c| c == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_with(major: u8, min: u8, max: u8, caps: &[&str]) -> Hello {
        Hello {
            binary_version: "test".into(),
            server_instance_id: "abc".into(),
            protocol: ProtocolRange {
                major,
                min_minor: min,
                max_minor: max,
            },
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            limits: Limits::current(),
            domain: None,
        }
    }

    fn required() -> Vec<&'static str> {
        REQUIRED_CAPS.to_vec()
    }

    #[test]
    fn major_mismatch_is_hard_reject() {
        let local = hello_with(2, 0, 3, &required());
        let remote = hello_with(1, 0, 0, &required());
        let err = negotiate(&local, &remote).unwrap_err();
        assert!(matches!(
            err,
            NegotiateError::MajorMismatch { remote: 1, .. }
        ));
    }

    #[test]
    fn minor_picks_highest_overlap() {
        let local = hello_with(2, 1, 4, &required());
        let remote = hello_with(2, 0, 3, &required());
        let got = negotiate(&local, &remote).unwrap();
        assert_eq!(got.minor, 3);
    }

    #[test]
    fn missing_required_cap_rejects() {
        let local = hello_with(2, 0, 0, &required());
        let remote = hello_with(2, 0, 0, &[CAP_DOMAIN_FRAME_V1]);
        let err = negotiate(&local, &remote).unwrap_err();
        match err {
            NegotiateError::MissingRequired(caps) => {
                assert!(caps.contains(&CAP_TARGETED_PANE_V1.to_string()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn optional_caps_are_intersected_not_required() {
        let mut caps = required();
        caps.push(CAP_BLOB_V1);
        let local = hello_with(2, 0, 0, &caps);
        let remote = hello_with(2, 0, 0, &required());
        let got = negotiate(&local, &remote).unwrap();
        assert!(!has_cap(&got.capabilities, CAP_BLOB_V1));
        assert!(has_cap(&got.capabilities, CAP_DOMAIN_FRAME_V1));
    }

    #[test]
    fn probe_hello_toc_tou_uses_hello_instance() {
        let probe = ProbeReport {
            server_instance_id: Some("old".into()),
            ..ProbeReport::not_running()
        };
        let hello = Hello::offer("new", None, &[]);
        assert_ne!(
            probe.server_instance_id.as_deref(),
            Some(hello.server_instance_id.as_str())
        );
    }
}
