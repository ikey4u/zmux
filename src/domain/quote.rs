use std::io;

pub fn posix_quote(arg: &str) -> io::Result<String> {
    if arg.contains('\0') || arg.contains('\n') || arg.contains('\r') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote command argument contains NUL or newline",
        ));
    }
    if arg.is_empty() {
        return Ok("''".to_string());
    }
    let safe = arg.bytes().all(|b| {
        matches!(
            b,
            b'a'..=b'z'
                | b'A'..=b'Z'
                | b'0'..=b'9'
                | b'.'
                | b'_'
                | b'-'
                | b'/'
                | b':'
                | b'@'
                | b'+'
                | b'='
        )
    });
    if safe {
        return Ok(arg.to_string());
    }
    Ok(format!("'{}'", arg.replace('\'', "'\\''")))
}

pub fn join_quoted(args: &[&str]) -> io::Result<String> {
    let mut out = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&posix_quote(arg)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_are_unquoted() {
        assert_eq!(posix_quote("zmux").unwrap(), "zmux");
        assert_eq!(posix_quote("/opt/zmux").unwrap(), "/opt/zmux");
    }

    #[test]
    fn spaces_and_quotes_are_shell_safe() {
        assert_eq!(posix_quote("foo bar").unwrap(), "'foo bar'");
        assert_eq!(posix_quote("it's").unwrap(), "'it'\\''s'");
    }

    #[test]
    fn nul_and_newline_are_rejected() {
        assert!(posix_quote("a\0b").is_err());
        assert!(posix_quote("a\nb").is_err());
    }

    #[test]
    fn join_builds_remote_mux_command() {
        let cmd = join_quoted(&[
            "zmux",
            "mux",
            "--stdio",
            "--start-if-missing",
            "--socket",
            "default",
        ])
        .unwrap();
        assert_eq!(cmd, "zmux mux --stdio --start-if-missing --socket default");
    }
}
