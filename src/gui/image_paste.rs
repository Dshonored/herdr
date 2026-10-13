//! Pasting clipboard images into a terminal pane.
//!
//! Terminals only carry text, so an image on the clipboard is saved as a file on
//! the machine that runs the pane and its path is pasted instead. Coding agents
//! such as Claude Code, Codex, and Pi attach an image when its path is pasted.

use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::ImageFormat;

/// Directory under the remote machine's `/tmp` that receives pasted images.
const REMOTE_DIR: &str = "/tmp/herdr-pasted-images";
const LOCAL_DIR: &str = "herdr-pasted-images";

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub(crate) fn extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
    }
}

/// A unique file name made only of `[A-Za-z0-9._-]`, safe in a shell command.
fn file_name(extension: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "pasted-{millis}-{}-{sequence}.{extension}",
        std::process::id()
    )
}

/// Text pasted into the pane for an image saved at `path`.
pub(crate) fn paste_text(path: &str) -> String {
    if path.chars().any(char::is_whitespace) && !cfg!(windows) {
        format!("'{}'", path.replace('\'', r"'\''"))
    } else {
        path.to_string()
    }
}

/// Saves the image on this machine and returns its path.
pub(crate) fn save_local(bytes: &[u8], extension: &str) -> io::Result<String> {
    let dir: PathBuf = std::env::temp_dir().join(LOCAL_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file_name(extension));
    std::fs::write(&path, bytes)?;
    Ok(path.to_string_lossy().into_owned())
}

fn remote_script(name: &str) -> String {
    format!(
        "umask 077 && mkdir -p {REMOTE_DIR} && cat > {REMOTE_DIR}/{name} && printf '%s' {REMOTE_DIR}/{name}"
    )
}

/// Uploads the image to the saved SSH machine `target` and returns its remote path.
pub(crate) fn upload_remote(target: &str, bytes: &[u8], extension: &str) -> io::Result<String> {
    let name = file_name(extension);
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            target,
            &remote_script(&name),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(bytes)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "upload to {target} failed: {}",
            stderr.trim()
        )));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(io::Error::other(format!(
            "upload to {target} returned no path"
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_unique_and_shell_safe() {
        let first = file_name("png");
        let second = file_name("png");
        assert_ne!(first, second);
        assert!(first.ends_with(".png"));
        assert!(first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')));
    }

    #[test]
    fn plain_paths_paste_verbatim() {
        assert_eq!(paste_text("/tmp/a/b.png"), "/tmp/a/b.png");
    }

    #[cfg(unix)]
    #[test]
    fn paths_with_spaces_are_quoted() {
        assert_eq!(paste_text("/tmp/my dir/a.png"), "'/tmp/my dir/a.png'");
        assert_eq!(paste_text("/tmp/it's here.png"), r"'/tmp/it'\''s here.png'");
    }

    #[test]
    fn remote_script_writes_and_prints_the_path() {
        let script = remote_script("pasted-1.png");
        assert!(script.contains("cat > /tmp/herdr-pasted-images/pasted-1.png"));
        assert!(script.ends_with("printf '%s' /tmp/herdr-pasted-images/pasted-1.png"));
    }

    #[test]
    fn saves_local_images() {
        let path = save_local(b"\x89PNG", "png").expect("save");
        assert_eq!(std::fs::read(&path).expect("read"), b"\x89PNG");
        let _ = std::fs::remove_file(path);
    }
}
