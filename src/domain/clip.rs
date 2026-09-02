use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::domain::{drop, paste::validate_paste_text, quote::posix_quote};

pub const MAX_IMAGE_SIDE: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_ZSYNC_OUTPUT_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum ClipboardItem {
    Text(String),
    ImagePng { bytes: Vec<u8>, name: String },
    Files(Vec<PathBuf>),
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
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        if let Ok(text) = clipboard.get_text() {
            if let Some(item) = clipboard_item_from_text(&text)? {
                return Ok(item);
            }
        }
        if let Ok(image) = clipboard.get_image() {
            return encode_clipboard_image(image);
        }
    }
    match read_zsync_clipboard() {
        Ok(item) => return Ok(item),
        Err(error) => {
            return Err(format!("clipboard is unavailable: {error}"));
        }
    }
}

fn clipboard_item_from_text(
    text: &str,
) -> Result<Option<ClipboardItem>, String> {
    if looks_like_uri_list(text) {
        let files = parse_uri_list(text)?;
        if !files.is_empty() {
            return Ok(Some(ClipboardItem::Files(files)));
        }
    }
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ClipboardItem::Text(text.to_string())))
    }
}

const ZSYNC_TIMEOUT: Duration = Duration::from_millis(1500);

fn zsync_command() -> Command {
    Command::new(zsync_bin())
}

fn zsync_bin() -> PathBuf {
    extra_zsync_dirs()
        .into_iter()
        .map(|dir| dir.join("zsync"))
        .chain(path_lookup("zsync"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("zsync"))
}

fn extra_zsync_dirs() -> Vec<PathBuf> {
    let Some(home) = crate::config::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".local").join("bin"),
        home.join(".cargo").join("bin"),
    ]
}

fn path_lookup(name: &str) -> impl Iterator<Item = PathBuf> {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths)
        .map(move |dir| dir.join(name))
        .collect::<Vec<_>>()
        .into_iter()
}

/// Copy text through zsync so a headless remote does not depend on OSC 52 / X11.
pub fn copy_via_zsync(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut cmd = zsync_command();
    cmd.arg("c")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_windows_console(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
    }
    wait_child(&mut child, ZSYNC_TIMEOUT)
}

/// Read the synchronized clipboard on this host.
///
/// Plain `zsync p` deliberately has two output modes: text is written to
/// stdout, while images/files are materialized in the current directory and
/// their absolute paths are written to stdout. Running it in zmux's private
/// drop directory gives the focused Linux pane a path that is valid on the
/// Linux host instead of a path from the desktop that originated the copy.
pub fn read_zsync_clipboard() -> Result<ClipboardItem, String> {
    let drop_dir = drop::ensure_drop_dir().map_err(|e| e.to_string())?;
    drop::gc_expired(&drop_dir).map_err(|e| e.to_string())?;

    let mut cmd = zsync_command();
    cmd.arg("p")
        .current_dir(&drop_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_windows_console(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run zsync paste: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture zsync paste output".to_string())?;

    // Drain stdout while the child is running. Waiting first can deadlock when
    // a large text clipboard fills the OS pipe buffer.
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        stdout
            .by_ref()
            .take(MAX_ZSYNC_OUTPUT_BYTES + 1)
            .read_to_end(&mut buf)
            .map(|_| buf)
    });
    if !wait_child(&mut child, ZSYNC_TIMEOUT) {
        let _ = reader.join();
        return Err("zsync paste timed out or failed".into());
    }
    let buf = reader
        .join()
        .map_err(|_| "zsync paste output reader failed".to_string())?
        .map_err(|e| format!("failed to read zsync paste output: {e}"))?;
    if buf.len() as u64 > MAX_ZSYNC_OUTPUT_BYTES {
        return Err("zsync clipboard text exceeds 10MiB".into());
    }
    let text = String::from_utf8(buf)
        .map_err(|_| "zsync paste returned invalid UTF-8".to_string())?;
    if text.is_empty() {
        return Err("zsync clipboard is empty".into());
    }

    zsync_item_from_output(&text, &drop_dir)
}

fn zsync_item_from_output(
    output: &str,
    drop_dir: &Path,
) -> Result<ClipboardItem, String> {
    if let Some(paths) = zsync_output_paths(output, drop_dir)? {
        return Ok(ClipboardItem::Files(paths));
    }
    Ok(ClipboardItem::Text(output.to_string()))
}

fn zsync_output_paths(
    output: &str,
    drop_dir: &Path,
) -> Result<Option<Vec<PathBuf>>, String> {
    let canonical_drop = fs::canonicalize(drop_dir)
        .map_err(|e| format!("failed to resolve zmux drop directory: {e}"))?;
    let mut paths = Vec::new();
    for line in output.lines() {
        if line.is_empty() || !Path::new(line).is_absolute() {
            return Ok(None);
        }
        let Ok(path) = fs::canonicalize(line) else {
            // Absolute clipboard text is still text unless it names a file
            // that zsync just created inside the private drop directory.
            return Ok(None);
        };
        if !path.starts_with(&canonical_drop) {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&path).map_err(|e| {
            format!("failed to inspect synced clipboard file: {e}")
        })?;
        if !metadata.file_type().is_file() {
            return Err("zsync paste returned a non-regular file".into());
        }
        if metadata.len() > drop::MAX_FILE_BYTES {
            return Err("synced clipboard file exceeds 64MiB".into());
        }
        paths.push(path);
        if paths.len() > drop::MAX_FILES {
            return Err("zsync clipboard has too many files (max 128)".into());
        }
    }
    if paths.is_empty() {
        Ok(None)
    } else {
        Ok(Some(paths))
    }
}

fn hide_windows_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

fn wait_child(child: &mut std::process::Child, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => thread::sleep(Duration::from_millis(15)),
            Err(_) => return false,
        }
    }
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

    #[test]
    fn extra_zsync_dirs_live_under_home() {
        let dirs = extra_zsync_dirs();
        if crate::config::home_dir().is_some() {
            assert!(dirs.iter().any(
                |d| d.ends_with(std::path::Path::new(".local").join("bin"))
            ));
            assert!(dirs.iter().any(
                |d| d.ends_with(std::path::Path::new(".cargo").join("bin"))
            ));
        }
    }

    #[test]
    fn clipboard_item_from_text_reads_plain_and_uri_list() {
        let item = clipboard_item_from_text("hello").unwrap().unwrap();
        assert!(matches!(item, ClipboardItem::Text(t) if t == "hello"));
        let item = clipboard_item_from_text("file:///tmp/a\n/tmp/b")
            .unwrap()
            .unwrap();
        let ClipboardItem::Files(files) = item else {
            panic!("expected files");
        };
        assert_eq!(files.len(), 2);
        assert!(clipboard_item_from_text("").unwrap().is_none());
    }

    #[test]
    fn zsync_output_only_becomes_paths_inside_drop_dir() {
        let dir = std::env::temp_dir().join(format!(
            "zmux-zsync-test-{}",
            crate::domain::ids::new_instance_id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let image = dir.join("clipboard image.png");
        fs::write(&image, b"png").unwrap();

        let output = format!("{}\n", image.display());
        let item = zsync_item_from_output(&output, &dir).unwrap();
        let ClipboardItem::Files(paths) = item else {
            panic!("expected synced file paths");
        };
        assert_eq!(paths, vec![fs::canonicalize(&image).unwrap()]);

        let item = zsync_item_from_output("/not/a/synced/file", &dir).unwrap();
        assert!(
            matches!(item, ClipboardItem::Text(text) if text == "/not/a/synced/file")
        );

        fs::remove_file(image).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
