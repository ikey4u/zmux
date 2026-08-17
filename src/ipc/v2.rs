use std::io::{self, Read, Write};

/// Cloud Domain stream magic. Local Unix v1 never uses this path.
pub const MAGIC: &[u8; 4] = b"ZMX2";
pub const PROTOCOL_MAJOR: u8 = 2;
pub const PROTOCOL_MIN_MINOR: u8 = 0;
pub const PROTOCOL_MAX_MINOR: u8 = 0;
pub const HEADER_LEN: usize = 26;

pub const MAX_ENVELOPE_PAYLOAD: u32 = 8 * 1024 * 1024;
pub const MAX_FRAME_METADATA: u32 = 1024 * 1024;
pub const MAX_BLOB_CHUNK: u32 = 256 * 1024;
pub const MAX_SEND_MEMORY: usize = 8 * 1024 * 1024;
pub const BLOB_FAIRNESS_BYTES: usize = 64 * 1024;
pub const INITIAL_BLOB_CREDIT: u32 = 8 * 1024 * 1024;

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Hello = 1,
    Incompatible = 2,
    Attach = 3,
    DomainFrame = 4,
    Input = 5,
    Paste = 6,
    Cmd = 7,
    Resp = 8,
    Resize = 9,
    Refresh = 10,
    Clip = 11,
    ClipText = 12,
    Blob = 13,
    WindowUpdate = 14,
    Cancel = 15,
    DropOk = 16,
}

impl MsgType {
    pub fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Incompatible,
            3 => Self::Attach,
            4 => Self::DomainFrame,
            5 => Self::Input,
            6 => Self::Paste,
            7 => Self::Cmd,
            8 => Self::Resp,
            9 => Self::Resize,
            10 => Self::Refresh,
            11 => Self::Clip,
            12 => Self::ClipText,
            13 => Self::Blob,
            14 => Self::WindowUpdate,
            15 => Self::Cancel,
            16 => Self::DropOk,
            _ => return None,
        })
    }

    pub fn as_u16(self) -> u16 {
        self as u16
    }

    pub fn class(self) -> MsgClass {
        match self {
            Self::Input
            | Self::Paste
            | Self::Cmd
            | Self::Resp
            | Self::Hello
            | Self::Incompatible
            | Self::Attach
            | Self::Resize
            | Self::Refresh
            | Self::Cancel
            | Self::WindowUpdate => MsgClass::Interactive,
            Self::DomainFrame | Self::ClipText => MsgClass::Frame,
            Self::Clip | Self::Blob | Self::DropOk => MsgClass::Blob,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgClass {
    Interactive,
    Frame,
    Blob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub major: u8,
    pub minor: u8,
    pub msg_type: u16,
    pub flags: u16,
    pub stream_id: u32,
    pub sequence: u64,
    pub payload: Vec<u8>,
}

impl Envelope {
    pub fn new(msg_type: MsgType, payload: Vec<u8>) -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MAX_MINOR,
            msg_type: msg_type.as_u16(),
            flags: 0,
            stream_id: 0,
            sequence: 0,
            payload,
        }
    }

    pub fn json(msg_type: MsgType, value: &impl serde::Serialize) -> Self {
        let payload =
            serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self::new(msg_type, payload)
    }

    pub fn typed(&self) -> Option<MsgType> {
        MsgType::from_u16(self.msg_type)
    }
}

#[derive(Debug)]
pub enum V2Error {
    Io(io::Error),
    BadMagic { found: [u8; 4] },
    Truncated(&'static str),
    PayloadTooLarge { declared: u32, max: u32 },
    UnknownType(u16),
}

impl std::fmt::Display for V2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::BadMagic { found } => {
                write!(
                    f,
                    "cloud stream stdout was polluted (expected ZMX2, got {})",
                    pretty_magic(*found)
                )
            }
            Self::Truncated(part) => {
                write!(f, "truncated v2 {part}")
            }
            Self::PayloadTooLarge { declared, max } => {
                write!(f, "v2 payload {declared} exceeds hard limit {max}")
            }
            Self::UnknownType(ty) => write!(f, "unknown v2 type {ty}"),
        }
    }
}

impl std::error::Error for V2Error {}

impl From<io::Error> for V2Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<V2Error> for io::Error {
    fn from(value: V2Error) -> Self {
        match value {
            V2Error::Io(err) => err,
            other => {
                io::Error::new(io::ErrorKind::InvalidData, other.to_string())
            }
        }
    }
}

fn pretty_magic(found: [u8; 4]) -> String {
    if found.iter().all(|b| (0x20..0x7f).contains(b)) {
        format!("{:?}", String::from_utf8_lossy(&found))
    } else {
        format!("{found:?}")
    }
}

pub fn write_envelope(
    writer: &mut impl Write,
    env: &Envelope,
) -> io::Result<()> {
    if env.payload.len() > MAX_ENVELOPE_PAYLOAD as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "v2 payload {} exceeds hard limit {}",
                env.payload.len(),
                MAX_ENVELOPE_PAYLOAD
            ),
        ));
    }
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(MAGIC);
    header[4] = env.major;
    header[5] = env.minor;
    header[6..8].copy_from_slice(&env.msg_type.to_be_bytes());
    header[8..10].copy_from_slice(&env.flags.to_be_bytes());
    header[10..14].copy_from_slice(&env.stream_id.to_be_bytes());
    header[14..22].copy_from_slice(&env.sequence.to_be_bytes());
    let len = env.payload.len() as u32;
    header[22..26].copy_from_slice(&len.to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(&env.payload)?;
    writer.flush()
}

pub fn read_envelope(reader: &mut impl Read) -> Result<Envelope, V2Error> {
    read_envelope_limited(reader, MAX_ENVELOPE_PAYLOAD)
}

pub fn read_envelope_limited(
    reader: &mut impl Read,
    max_payload: u32,
) -> Result<Envelope, V2Error> {
    let mut header = [0u8; HEADER_LEN];
    let mut filled = 0usize;
    while filled < HEADER_LEN {
        match reader.read(&mut header[filled..]) {
            Ok(0) => {
                if filled >= 4 && &header[0..4] != MAGIC {
                    let mut found = [0u8; 4];
                    found.copy_from_slice(&header[0..4]);
                    return Err(V2Error::BadMagic { found });
                }
                if filled > 0 && header[0] != MAGIC[0] {
                    let mut found = [0u8; 4];
                    let n = filled.min(4);
                    found[..n].copy_from_slice(&header[..n]);
                    return Err(V2Error::BadMagic { found });
                }
                return Err(V2Error::Truncated("header"));
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(V2Error::Io(err)),
        }
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&header[0..4]);
    if &magic != MAGIC {
        return Err(V2Error::BadMagic { found: magic });
    }
    let major = header[4];
    let minor = header[5];
    let msg_type = u16::from_be_bytes([header[6], header[7]]);
    let flags = u16::from_be_bytes([header[8], header[9]]);
    let stream_id =
        u32::from_be_bytes([header[10], header[11], header[12], header[13]]);
    let sequence = u64::from_be_bytes([
        header[14], header[15], header[16], header[17], header[18], header[19],
        header[20], header[21],
    ]);
    let payload_len =
        u32::from_be_bytes([header[22], header[23], header[24], header[25]]);
    if payload_len > max_payload {
        return Err(V2Error::PayloadTooLarge {
            declared: payload_len,
            max: max_payload,
        });
    }
    let mut payload = vec![0u8; payload_len as usize];
    if payload_len > 0 {
        match reader.read_exact(&mut payload) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(V2Error::Truncated("payload"));
            }
            Err(err) => return Err(V2Error::Io(err)),
        }
    }
    Ok(Envelope {
        major,
        minor,
        msg_type,
        flags,
        stream_id,
        sequence,
        payload,
    })
}

/// Fair-queued writer: interactive ACK/input, then frames, then BLOB
/// with a per-round byte quota. Credit is required before BLOB bytes leave.
pub struct PriorityWriter<W: Write> {
    inner: W,
    interactive: std::collections::VecDeque<Envelope>,
    frames: std::collections::VecDeque<Envelope>,
    blobs: std::collections::VecDeque<Envelope>,
    blob_credit: u32,
    queued_bytes: usize,
}

impl<W: Write> PriorityWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            interactive: std::collections::VecDeque::new(),
            frames: std::collections::VecDeque::new(),
            blobs: std::collections::VecDeque::new(),
            blob_credit: INITIAL_BLOB_CREDIT,
            queued_bytes: 0,
        }
    }

    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    pub fn can_enqueue(&self, env: &Envelope) -> bool {
        let size = HEADER_LEN + env.payload.len();
        self.queued_bytes.saturating_add(size) <= MAX_SEND_MEMORY
    }

    pub fn enqueue(&mut self, env: Envelope) -> io::Result<()> {
        if !self.can_enqueue(&env) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "domain send memory window exhausted",
            ));
        }
        let size = HEADER_LEN + env.payload.len();
        self.queued_bytes += size;
        match MsgType::from_u16(env.msg_type).map(MsgType::class) {
            Some(MsgClass::Interactive) => self.interactive.push_back(env),
            Some(MsgClass::Frame) => self.frames.push_back(env),
            Some(MsgClass::Blob) => self.blobs.push_back(env),
            None => self.interactive.push_back(env),
        }
        Ok(())
    }

    pub fn add_credit(&mut self, credit: u32) {
        self.blob_credit = self.blob_credit.saturating_add(credit);
    }

    pub fn flush_ready(&mut self) -> io::Result<usize> {
        let mut wrote = 0usize;
        while let Some(env) = self.interactive.pop_front() {
            wrote += self.write_one(env)?;
        }
        if let Some(env) = self.frames.pop_front() {
            wrote += self.write_one(env)?;
        }
        let mut blob_budget = BLOB_FAIRNESS_BYTES;
        while blob_budget > 0 {
            let Some(env) = self.blobs.front() else {
                break;
            };
            if env.msg_type == MsgType::Blob as u16 {
                let need = env.payload.len() as u32;
                if need > self.blob_credit {
                    break;
                }
            }
            let env = self.blobs.pop_front().unwrap();
            let size = env.payload.len();
            if env.msg_type == MsgType::Blob as u16 {
                self.blob_credit = self.blob_credit.saturating_sub(size as u32);
            }
            blob_budget = blob_budget.saturating_sub(HEADER_LEN + size);
            wrote += self.write_one(env)?;
        }
        Ok(wrote)
    }

    fn write_one(&mut self, env: Envelope) -> io::Result<usize> {
        let size = HEADER_LEN + env.payload.len();
        self.queued_bytes = self.queued_bytes.saturating_sub(size);
        write_envelope(&mut self.inner, &env)?;
        Ok(size)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn roundtrip(env: Envelope) -> Envelope {
        let mut buf = Vec::new();
        write_envelope(&mut buf, &env).unwrap();
        read_envelope(&mut Cursor::new(buf)).unwrap()
    }

    #[test]
    fn envelope_roundtrip_preserves_fields() {
        let mut env = Envelope::new(MsgType::Input, b"hello".to_vec());
        env.flags = 3;
        env.stream_id = 9;
        env.sequence = 42;
        let got = roundtrip(env.clone());
        assert_eq!(got, env);
    }

    #[test]
    fn sticky_packets_are_read_in_order() {
        let mut buf = Vec::new();
        write_envelope(&mut buf, &Envelope::new(MsgType::Hello, b"a".to_vec()))
            .unwrap();
        write_envelope(&mut buf, &Envelope::new(MsgType::Cmd, b"b".to_vec()))
            .unwrap();
        let mut cur = Cursor::new(buf);
        let first = read_envelope(&mut cur).unwrap();
        let second = read_envelope(&mut cur).unwrap();
        assert_eq!(first.payload, b"a");
        assert_eq!(second.payload, b"b");
        assert_eq!(first.typed(), Some(MsgType::Hello));
        assert_eq!(second.typed(), Some(MsgType::Cmd));
    }

    #[test]
    fn truncated_header_is_an_error() {
        let err = read_envelope(&mut Cursor::new(&b"ZMX"[..])).unwrap_err();
        assert!(matches!(err, V2Error::Truncated("header")));
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let mut buf = Vec::new();
        write_envelope(&mut buf, &Envelope::new(MsgType::Hello, vec![1, 2, 3]))
            .unwrap();
        buf.pop();
        let err = read_envelope(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, V2Error::Truncated("payload")));
    }

    #[test]
    fn oversized_length_is_rejected_before_allocating() {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(MAGIC);
        header[4] = PROTOCOL_MAJOR;
        header[22..26]
            .copy_from_slice(&(MAX_ENVELOPE_PAYLOAD + 1).to_be_bytes());
        let err = read_envelope(&mut Cursor::new(header)).unwrap_err();
        match err {
            V2Error::PayloadTooLarge { declared, max } => {
                assert_eq!(declared, MAX_ENVELOPE_PAYLOAD + 1);
                assert_eq!(max, MAX_ENVELOPE_PAYLOAD);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn polluted_stdout_fails_the_handshake() {
        let err =
            read_envelope(&mut Cursor::new(b"Welcome to Linux\n")).unwrap_err();
        assert!(matches!(err, V2Error::BadMagic { .. }));
        assert!(err.to_string().contains("polluted"));
    }

    #[test]
    fn interactive_messages_outrank_queued_blobs() {
        let mut writer = PriorityWriter::new(Vec::new());
        writer
            .enqueue(Envelope::new(MsgType::Blob, vec![1; 8]))
            .unwrap();
        writer
            .enqueue(Envelope::new(MsgType::Input, b"i".to_vec()))
            .unwrap();
        writer
            .enqueue(Envelope::new(MsgType::DomainFrame, b"f".to_vec()))
            .unwrap();
        writer.flush_ready().unwrap();
        let mut cur = Cursor::new(writer.inner);
        assert_eq!(
            read_envelope(&mut cur).unwrap().typed(),
            Some(MsgType::Input)
        );
        assert_eq!(
            read_envelope(&mut cur).unwrap().typed(),
            Some(MsgType::DomainFrame)
        );
        assert_eq!(
            read_envelope(&mut cur).unwrap().typed(),
            Some(MsgType::Blob)
        );
    }

    #[test]
    fn blob_waits_for_credit_window() {
        let mut writer = PriorityWriter::new(Vec::new());
        writer.blob_credit = 0;
        writer
            .enqueue(Envelope::new(MsgType::Blob, vec![1, 2, 3]))
            .unwrap();
        writer.flush_ready().unwrap();
        assert!(writer.inner.is_empty());
        writer.add_credit(3);
        writer.flush_ready().unwrap();
        assert!(!writer.inner.is_empty());
    }

    #[test]
    fn enqueue_rejects_when_send_window_is_full() {
        let mut writer = PriorityWriter::new(Vec::new());
        let chunk =
            Envelope::new(MsgType::Blob, vec![0; MAX_BLOB_CHUNK as usize]);
        let mut accepted = 0usize;
        while writer.can_enqueue(&chunk) {
            writer.enqueue(chunk.clone()).unwrap();
            accepted += 1;
            if accepted > 64 {
                break;
            }
        }
        assert!(!writer.can_enqueue(&chunk));
        assert!(writer.enqueue(chunk).is_err());
        assert!(accepted >= 1);
    }
}
