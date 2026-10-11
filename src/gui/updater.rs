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
        ("macos", "aarch64") => "herdr-desktop-macos-aarch64.zip",
        ("macos", "x86_64") => "herdr-desktop-macos-x86_64.zip",
        ("linux", "x86_64") => "herdr-desktop-linux-x86_64.tar.gz",
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

/// What an update replaces: the `.app` bundle when running from one (any name, as the
/// user may have renamed it), otherwise the folder holding the executable.
fn install_root_for(exe: &Path) -> Option<PathBuf> {
    let parent = exe.parent()?;
    let bundle = parent
        .ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"));
    Some(bundle.unwrap_or(parent).to_path_buf())
}

fn install_dir() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("can't find this program: {error}"))?;
    if is_translocated(&exe) {
        return Err(
            "macOS is running herdr from a temporary read-only copy because it was \
             opened outside Applications. Install it with the herdr .pkg (or move herdr.app \
             to Applications), then reopen it."
                .into(),
        );
    }
    install_root_for(&exe).ok_or_else(|| "can't find the install folder".into())
}

/// Gatekeeper runs quarantined apps opened from Downloads and similar folders from a
/// randomized read-only mount that disappears later; nothing there can be updated.
fn is_translocated(exe: &Path) -> bool {
    exe.components()
        .any(|part| part.as_os_str() == "AppTranslocation")
}

/// The folder whose contents map onto the install root. Archives that wrap the app in one
/// folder (`herdr.app` on macOS, `herdr-desktop` on Linux) map that folder; the Windows
/// zip maps everything unpacked.
fn staged_root(unpacked: &Path) -> std::io::Result<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(unpacked)?.collect::<Result<_, _>>()?;
    if let [entry] = entries.as_slice() {
        if entry.file_type()?.is_dir() {
            return Ok(entry.path());
        }
    }
    Ok(unpacked.to_path_buf())
}

/// Files ready to replace the installed ones.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StagedUpdate {
    pub(crate) release: Release,
    dir: PathBuf,
    root: PathBuf,
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
    if asset.ends_with(".tar.gz") {
        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&download)
            .arg("-C")
            .arg(&files)
            .status()
            .map_err(|error| format!("could not unpack the update: {error}"))?;
        if !status.success() {
            return Err("could not unpack the update".into());
        }
    } else if asset.ends_with(".zip") {
        let mut unpack = if Path::new(DITTO).is_file() {
            // ditto keeps a macOS bundle's permissions and symlinks.
            let mut command = std::process::Command::new(DITTO);
            command.arg("-x").arg("-k").arg(&download).arg(&files);
            command
        } else {
            let mut command = std::process::Command::new(zip_tar());
            command.arg("-xf").arg(&download).arg("-C").arg(&files);
            command
        };
        let status = unpack
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
    let root = staged_root(&files).map_err(|e| e.to_string())?;
    Ok(StagedUpdate {
        release: release.clone(),
        dir,
        root,
    })
}

const DITTO: &str = "/usr/bin/ditto";

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
    install_into(&install, &staged.root)?;
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
                let known = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        matches!(
                            name,
                            "conpty" | "Contents" | "MacOS" | "Resources" | "_CodeSignature"
                        )
                    });
                if known {
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
    fn updates_replace_the_app_bundle_or_the_executable_folder() {
        let bundle = Path::new("/Applications/My herdr.app/Contents/MacOS/herdr");
        assert_eq!(
            install_root_for(bundle).unwrap(),
            PathBuf::from("/Applications/My herdr.app")
        );
        let plain = Path::new("/opt/herdr/herdr");
        assert_eq!(
            install_root_for(plain).unwrap(),
            PathBuf::from("/opt/herdr")
        );
    }

    #[test]
    fn translocated_copies_are_not_updatable() {
        assert!(is_translocated(Path::new(
            "/private/var/folders/ls/x/T/AppTranslocation/3877A32F/d/herdr.app/Contents/MacOS/herdr"
        )));
        assert!(!is_translocated(Path::new(
            "/Applications/herdr.app/Contents/MacOS/herdr"
        )));
    }

    #[test]
    fn a_single_unpacked_bundle_is_the_staged_root() {
        let root = std::env::temp_dir().join(format!("herdr-staged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("herdr.app").join("Contents")).unwrap();
        assert_eq!(staged_root(&root).unwrap(), root.join("herdr.app"));
        // A Windows zip unpacks several entries and maps as a whole.
        std::fs::write(root.join("README"), "").unwrap();
        assert_eq!(staged_root(&root).unwrap(), root);
        let _ = std::fs::remove_dir_all(&root);
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
