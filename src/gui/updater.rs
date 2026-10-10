//! Desktop app updates from GitHub Releases.
//!
//! Releases are tagged `desktop-v<semver>` and carry one asset per platform plus a
//! `SHA256SUMS` file. Updating downloads and verifies the asset next to the running
//! executable, then swaps files in place: the running files are renamed aside first, which
//! Windows allows for loaded executables and DLLs, and are cleaned up on the next launch.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::update::Version;

pub(crate) const TAG_PREFIX: &str = "desktop-v";
const DEFAULT_REPO: &str = "Dshonored/herdr";
const STAGING_DIR: &str = ".herdr-desktop-update";
const OLD_SUFFIX: &str = ".herdr-old";

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(Release),
    Ready(StagedUpdate),
    Installing,
    Failed(String),
}

/// This build's desktop version, set by the release workflow; `None` for local builds.
pub(crate) fn current_version() -> Option<Version> {
    option_env!("HERDR_DESKTOP_VERSION").and_then(Version::parse)
}

pub(crate) fn current_version_label() -> String {
    current_version().map_or_else(|| "dev build".into(), |version| version.to_string())
}

fn repo() -> &'static str {
    option_env!("HERDR_DESKTOP_REPO").unwrap_or(DEFAULT_REPO)
}

pub(crate) fn releases_page() -> String {
    format!("https://github.com/{}/releases", repo())
}

/// The release asset for this platform.
pub(crate) fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "herdr-desktop-windows-x86_64.zip",
        ("macos", "aarch64") => "herdr-desktop-macos-aarch64",
        ("macos", "x86_64") => "herdr-desktop-macos-x86_64",
        ("linux", "x86_64") => "herdr-desktop-linux-x86_64",
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Release {
    pub(crate) version: Version,
    pub(crate) tag: String,
    pub(crate) page: String,
    asset_url: String,
    checksums_url: Option<String>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// Newest published desktop release that has an asset for this platform.
fn pick_release(releases: Vec<GithubRelease>, asset: &str) -> Option<Release> {
    releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.strip_prefix(TAG_PREFIX)?)?;
            let find = |name: &str| {
                release
                    .assets
                    .iter()
                    .find(|candidate| candidate.name == name)
                    .map(|candidate| candidate.browser_download_url.clone())
            };
            Some(Release {
                version,
                asset_url: find(asset)?,
                checksums_url: find("SHA256SUMS"),
                tag: release.tag_name,
                page: release.html_url,
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

fn curl(url: &str) -> Result<Vec<u8>, String> {
    let output = crate::noninteractive_process::curl_command()
        .args([
            "-sfL",
            "--max-time",
            "30",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: herdr-desktop",
            url,
        ])
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !output.status.success() {
        return Err(format!("request failed: {url}"));
    }
    Ok(output.stdout)
}

/// Latest release for this platform, whether or not it is newer than this build.
pub(crate) fn fetch_latest() -> Result<Release, String> {
    let asset = asset_name().ok_or("no desktop builds are published for this platform")?;
    let body = curl(&format!(
        "https://api.github.com/repos/{}/releases?per_page=30",
        repo()
    ))?;
    let releases: Vec<GithubRelease> = serde_json::from_slice(&body)
        .map_err(|error| format!("unexpected GitHub reply: {error}"))?;
    pick_release(releases, asset).ok_or_else(|| format!("no desktop release has {asset} yet"))
}

pub(crate) fn is_newer(release: &Release) -> bool {
    current_version().is_none_or(|current| release.version > current)
}

fn install_dir() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("can't find this program: {error}"))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "can't find the install folder".into())
}

/// Files ready to replace the installed ones.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StagedUpdate {
    pub(crate) release: Release,
    dir: PathBuf,
}

fn expected_checksum(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset).then(|| hash.to_ascii_lowercase())
    })
}

/// Downloads and verifies the release into a staging folder beside the executable.
pub(crate) fn download(release: &Release) -> Result<StagedUpdate, String> {
    let asset = asset_name().ok_or("unsupported platform")?;
    let dir = install_dir()?.join(STAGING_DIR).join(&release.tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|error| {
        format!(
            "can't write to the install folder {} ({error})",
            dir.display()
        )
    })?;
    let download = dir.join(asset);
    std::fs::write(&download, curl(&release.asset_url)?).map_err(|e| e.to_string())?;

    let sums_url = release
        .checksums_url
        .as_ref()
        .ok_or("the release has no SHA256SUMS; refusing an unverified update")?;
    let sums = String::from_utf8_lossy(&curl(sums_url)?).into_owned();
    let expected =
        expected_checksum(&sums, asset).ok_or("SHA256SUMS does not list this platform's file")?;
    crate::checksum::verify_sha256(&download, &expected)
        .map_err(|error| format!("checksum mismatch, update discarded: {error}"))?;

    let files = dir.join("files");
    std::fs::create_dir_all(&files).map_err(|e| e.to_string())?;
    if asset.ends_with(".zip") {
        let status = std::process::Command::new(zip_tar())
            .arg("-xf")
            .arg(&download)
            .arg("-C")
            .arg(&files)
            .status()
            .map_err(|error| format!("could not unpack the update: {error}"))?;
        if !status.success() {
            return Err("could not unpack the update".into());
        }
    } else {
        let exe_name = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.file_name().map(ToOwned::to_owned))
            .ok_or("can't find this program's name")?;
        let target = files.join(exe_name);
        std::fs::rename(&download, &target).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(StagedUpdate {
        release: release.clone(),
        dir,
    })
}

/// Windows' bundled bsdtar (Windows 10 and later) reads zip archives. Name it by path:
/// a `tar` earlier on PATH, such as Git Bash's GNU tar, cannot.
fn zip_tar() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("tar.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("tar"))
}

fn staged_files(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            staged_files(root, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Moves the staged files over the installed ones. Running files are renamed aside.
pub(crate) fn install(staged: &StagedUpdate) -> Result<(), String> {
    let install = install_dir()?;
    install_into(&install, &staged.dir.join("files"))?;
    let _ = std::fs::remove_dir_all(install.join(STAGING_DIR));
    Ok(())
}

fn install_into(install: &Path, files_root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    staged_files(files_root, Path::new(""), &mut files).map_err(|e| e.to_string())?;
    if files.is_empty() {
        return Err("the downloaded update is empty".into());
    }
    let mut moved_aside = Vec::new();
    for relative in &files {
        let target = install.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if target.exists() {
            let aside = aside_path(&target);
            let _ = std::fs::remove_file(&aside);
            if let Err(error) = std::fs::rename(&target, &aside) {
                rollback(&moved_aside);
                return Err(format!("can't replace {}: {error}", target.display()));
            }
            moved_aside.push((target.clone(), aside));
        }
        if let Err(error) = std::fs::rename(files_root.join(relative), &target) {
            rollback(&moved_aside);
            return Err(format!("can't install {}: {error}", target.display()));
        }
    }
    Ok(())
}

fn aside_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(OLD_SUFFIX);
    path.with_file_name(name)
}

fn rollback(moved_aside: &[(PathBuf, PathBuf)]) {
    for (original, aside) in moved_aside.iter().rev() {
        let _ = std::fs::remove_file(original);
        let _ = std::fs::rename(aside, original);
    }
}

/// Removes files left aside by a previous update; in-use ones stay until a later launch.
pub(crate) fn cleanup_previous_update() {
    let Ok(install) = install_dir() else {
        return;
    };
    let mut stack = vec![install];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if path.file_name().is_some_and(|name| name == "conpty") {
                    stack.push(path);
                }
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(OLD_SUFFIX))
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Starts the updated app; the caller quits the current one.
pub(crate) fn relaunch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg("gui")
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not restart: {error}"))
}

/// Sets `[ui] max_fps` in this computer's herdr config, keeping comments and layout.
pub(crate) fn set_local_max_fps(fps: u16) -> Result<(), String> {
    let path = crate::config::config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| format!("{} is not valid TOML: {error}", path.display()))?;
    let ui = document
        .entry("ui")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .ok_or("[ui] in config.toml is not a table")?;
    ui["max_fps"] = toml_edit::value(i64::from(fps));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, document.to_string()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool, assets: &[&str]) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            html_url: format!("https://example.test/{tag}"),
            draft,
            prerelease: false,
            assets: assets
                .iter()
                .map(|name| GithubAsset {
                    name: (*name).into(),
                    browser_download_url: format!("https://example.test/{tag}/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn picks_the_newest_desktop_release_with_this_platforms_asset() {
        let asset = "herdr-desktop-linux-x86_64";
        let picked = pick_release(
            vec![
                release("desktop-v0.1.0", false, &[asset, "SHA256SUMS"]),
                release("desktop-v0.3.0", true, &[asset]),
                release("desktop-v0.2.0", false, &[asset, "SHA256SUMS"]),
                release("desktop-v0.4.0", false, &["herdr-desktop-macos-aarch64"]),
                release("v9.9.9", false, &[asset]),
            ],
            asset,
        )
        .unwrap();
        assert_eq!(picked.tag, "desktop-v0.2.0");
        assert!(picked.checksums_url.is_some());
    }

    #[test]
    fn checksum_lines_match_the_asset_name() {
        let sums = "abc123  herdr-desktop-windows-x86_64.zip\nDEF456 *herdr-desktop-linux-x86_64\n";
        assert_eq!(
            expected_checksum(sums, "herdr-desktop-linux-x86_64").as_deref(),
            Some("def456")
        );
        assert_eq!(expected_checksum(sums, "missing"), None);
    }

    #[test]
    fn install_swaps_files_and_keeps_the_old_ones_aside() {
        let root = std::env::temp_dir().join(format!("herdr-updater-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let install = root.join("install");
        let staged = root.join("staged");
        std::fs::create_dir_all(install.join("conpty")).unwrap();
        std::fs::create_dir_all(staged.join("files").join("conpty")).unwrap();
        std::fs::write(install.join("app"), "old").unwrap();
        std::fs::write(install.join("conpty").join("lib"), "old").unwrap();
        std::fs::write(staged.join("files").join("app"), "new").unwrap();
        std::fs::write(staged.join("files").join("conpty").join("lib"), "new").unwrap();

        install_into(&install, &staged.join("files")).unwrap();
        assert_eq!(std::fs::read_to_string(install.join("app")).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(install.join("conpty").join("lib")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(install.join("app.herdr-old")).unwrap(),
            "old"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
