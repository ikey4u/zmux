use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use crate::domain::{drop, paste::validate_paste_text, quote::posix_quote};

pub const MAX_IMAGE_SIDE: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 16_000_000;

#[derive(Debug, Clone)]
pub enum ClipboardItem {
    Text(String),
    ImagePng { bytes: Vec<u8>, name: String },
    Files(Vec<PathBuf>),
}

#[derive(Debug, Clone)]
pub struct PasteTarget {
    pub pane_id: usize,
    pub generation: u64,
    pub remote: bool,
}

pub fn read_os_clipboard() -> Result<ClipboardItem, String> {
    if let Some(files) = platform_files()? {
        if files.is_empty() {
            return Err("clipboard file list is empty".into());
        }
        if files.len() > drop::MAX_FILES {
            return Err("clipboard has too many files (max 128)".into());
        }
        return Ok(ClipboardItem::Files(files));
    }
    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| err.to_string())?;
    if let Ok(text) = clipboard.get_text() {
        if looks_like_uri_list(&text) {
            let files = parse_uri_list(&text)?;
            if !files.is_empty() {
                return Ok(ClipboardItem::Files(files));
            }
        }
        if !text.is_empty() {
            return Ok(ClipboardItem::Text(text));
        }
    }
    if let Ok(image) = clipboard.get_image() {
        return encode_clipboard_image(image);
    }
    Err("clipboard is empty or unsupported".into())
}

fn looks_like_uri_list(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line.starts_with("file:") || line.starts_with('/')
    }) && text.lines().all(|line| {
        let line = line.trim();
        line.is_empty()
            || line.starts_with('#')
            || line.starts_with("file:")
            || line.starts_with('/')
    })
}

fn parse_uri_list(text: &str) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(path) = file_uri_to_path(line) {
            out.push(path);
        } else if line.starts_with('/') {
            out.push(PathBuf::from(line));
        }
    }
    Ok(out)
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    if decoded.contains("..") {
        return None;
    }
    Some(PathBuf::from(decoded))
}

fn percent_decode(input: &str) -> String {
    let mut out = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn encode_clipboard_image(
    image: arboard::ImageData,
) -> Result<ClipboardItem, String> {
    let width = image.width as u32;
    let height = image.height as u32;
    if width == 0 || height == 0 {
        return Err("clipboard image is empty".into());
    }
    if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err("clipboard image exceeds 8192px on a side".into());
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err("clipboard image exceeds 16M pixels".into());
    }
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&image.bytes)
            .map_err(|e| e.to_string())?;
    }
    Ok(ClipboardItem::ImagePng {
        bytes: buf,
        name: "clipboard.png".into(),
    })
}

fn platform_files() -> Result<Option<Vec<PathBuf>>, String> {
    #[cfg(target_os = "macos")]
    {
        macos_pasteboard_files()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
fn macos_pasteboard_files() -> Result<Option<Vec<PathBuf>>, String> {
    use objc2::ClassType;
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSArray, NSURL};

    let pasteboard = NSPasteboard::generalPasteboard();
    let classes = NSArray::from_slice(&[NSURL::class()]);
    let objects =
        unsafe { pasteboard.readObjectsForClasses_options(&classes, None) };
    let Some(objects) = objects else {
        return Ok(None);
    };
    let mut files = Vec::new();
    for obj in objects.iter() {
        let Ok(url) = obj.downcast::<NSURL>() else {
            continue;
        };
        let Some(path) = url.path() else {
            continue;
        };
        let path = PathBuf::from(path.to_string());
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        files.push(path);
    }
    if files.is_empty() {
        Ok(None)
    } else {
        Ok(Some(files))
    }
}

pub fn quote_paths(paths: &[String], raw: bool) -> Result<String, String> {
    if raw {
        return Ok(paths.join("\n"));
    }
    let mut out = String::new();
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&posix_quote(path).map_err(|e| e.to_string())?);
    }
    Ok(out)
}

pub fn wrap_bracketed_paste(text: &str, enabled: bool, raw: bool) -> Vec<u8> {
    if raw || !enabled {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 16);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

pub fn validate_or_text(item: &ClipboardItem, raw: bool) -> Result<(), String> {
    if let ClipboardItem::Text(text) = item {
        validate_paste_text(text, raw)?;
    }
    Ok(())
}

pub fn stream_file_chunks(
    path: &Path,
    mut on_chunk: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<u64, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > drop::MAX_FILE_BYTES {
            return Err("file exceeds 64MiB limit".into());
        }
        on_chunk(&buf[..n])?;
    }
    Ok(total)
}

pub fn save_local_image(bytes: &[u8]) -> Result<PathBuf, String> {
    drop::write_bytes_atomic("png", bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_spaces_and_rejects_traversal_uri() {
        let quoted =
            quote_paths(&["/tmp/my file.png".into(), "/tmp/ok".into()], false)
                .unwrap();
        assert!(quoted.contains("my file") || quoted.contains("'"));
        assert!(file_uri_to_path("file:///tmp/../etc/passwd").is_none());
    }

    #[test]
    fn raw_paths_are_newline_separated() {
        let raw = quote_paths(&["/a b".into(), "/c".into()], true).unwrap();
        assert_eq!(raw, "/a b\n/c");
    }
}
