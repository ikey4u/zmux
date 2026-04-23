use std::io::{self, BufRead, BufReader, Read, Write};

pub const PROTOCOL_VERSION: u8 = 1;
pub const HANDSHAKE_MAGIC: &str = "ZMUX";

pub fn send_handshake(w: &mut dyn Write) -> io::Result<()> {
    write!(w, "{} {}\n", HANDSHAKE_MAGIC, PROTOCOL_VERSION)?;
    w.flush()
}

pub fn recv_handshake(r: &mut dyn Read) -> io::Result<()> {
    let mut br = BufReader::new(r);
    let mut line = String::new();
    br.read_line(&mut line)?;
    let line = line.trim();
    let expected = format!("{} {}", HANDSHAKE_MAGIC, PROTOCOL_VERSION);
    if line != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("handshake mismatch: got {:?}", line),
        ));
    }
    Ok(())
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
    let mut header = String::new();
    r.read_line(&mut header)?;
    let header = header.trim();
    let len: usize = header
        .strip_prefix("FRAME ")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "bad frame header")
        })?;
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
    let mut line = String::new();
    r.read_line(&mut line)?;
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
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        return String::from_utf8(buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
    }
    Ok(header)
}

pub fn send_resp(w: &mut dyn Write, data: &str) -> io::Result<()> {
    write!(w, "RESP {}\n", data.len())?;
    w.write_all(data.as_bytes())?;
    w.flush()
}
