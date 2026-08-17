pub fn validate_paste_text(text: &str, raw: bool) -> Result<(), String> {
    if raw {
        if text.contains('\0') {
            return Err("paste-raw still rejects NUL".to_string());
        }
        return Ok(());
    }
    if text.contains('\0') {
        return Err("paste rejected: NUL byte".to_string());
    }
    if text.contains("\u{1b}[201~") {
        return Err("paste rejected: bracketed-paste terminator".to_string());
    }
    for ch in text.chars() {
        let n = ch as u32;
        if n < 0x20 && !matches!(ch, '\t' | '\n' | '\r') {
            return Err(format!("paste rejected: control byte U+{n:04X}"));
        }
        if n == 0x7f {
            return Err("paste rejected: DEL".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_tab_and_newlines() {
        assert!(validate_paste_text("a\tb\n", false).is_ok());
    }

    #[test]
    fn rejects_escape_and_nul() {
        assert!(validate_paste_text("\u{1b}[31m", false).is_err());
        assert!(validate_paste_text("a\0b", false).is_err());
    }
}
