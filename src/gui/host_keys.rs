//! First-connection host key trust for SSH machines.
//!
//! The desktop app runs SSH without a terminal (BatchMode), so SSH cannot ask "Are you
//! sure you want to continue connecting?" and refuses machines it has never seen. This
//! module recognises that failure, scans the machine's key so the user can compare its
//! fingerprint, and records exactly that key in `~/.ssh/known_hosts` once they confirm.
//! A *changed* key is never offered for trust.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// What went wrong with an SSH connection, from SSH's own error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SshProblem {
    /// First connection: SSH has no key on record for this machine.
    UnknownHostKey,
    /// The machine presents a different key than the one on record.
    ChangedHostKey,
    UnknownHost,
    AuthFailed,
    Unreachable,
    Other,
}

pub(crate) fn classify(error: &str) -> SshProblem {
    if error.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || (error.contains("Host key for") && error.contains("has changed"))
    {
        SshProblem::ChangedHostKey
    } else if error.contains("host key is known for")
        || error.contains("Host key verification failed")
    {
        SshProblem::UnknownHostKey
    } else if error.contains("Could not resolve hostname") {
        SshProblem::UnknownHost
    } else if error.contains("Permission denied") {
        SshProblem::AuthFailed
    } else if error.contains("Connection timed out")
        || error.contains("Connection refused")
        || error.contains("No route to host")
        || error.contains("Network is unreachable")
    {
        SshProblem::Unreachable
    } else {
        SshProblem::Other
    }
}

/// A short explanation of a connection failure for the machine list.
pub(crate) fn explain(error: &str, target: &str) -> String {
    match classify(error) {
        SshProblem::UnknownHostKey => format!("First connection to {target}: confirm its host key"),
        SshProblem::ChangedHostKey => format!(
            "{target}'s host key changed. If the machine wasn't reinstalled, someone may be intercepting the connection; after checking, remove the old key with `ssh-keygen -R <host>`."
        ),
        SshProblem::UnknownHost => format!(
            "Can't find {target}. Check the name, or add it to ~/.ssh/config on this computer."
        ),
        SshProblem::AuthFailed => format!(
            "{target} refused the SSH login. The app needs key-based login (no password prompts); add your key with ssh-copy-id or load it with ssh-add."
        ),
        SshProblem::Unreachable => format!("Can't reach {target}: {}", last_line(error)),
        SshProblem::Other => error.to_owned(),
    }
}

fn last_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or(text)
}

/// A machine's host keys as scanned, ready to compare and record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScannedKey {
    /// The address SSH connects to, e.g. `100.127.93.17` or `[host]:2222`.
    pub(crate) host: String,
    /// `known_hosts` lines for exactly the keys scanned.
    entries: Vec<String>,
    /// e.g. `ED25519 SHA256:patyt…`.
    pub(crate) fingerprints: Vec<String>,
}

/// Where SSH really connects for `target`, honouring ~/.ssh/config (`ssh -G`).
struct Resolved {
    host: String,
    port: u16,
    known_hosts_name: String,
}

fn resolved_from_config(config: &str) -> Option<Resolved> {
    let mut host = None;
    let mut port = 22;
    let mut alias = None;
    for line in config.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "hostname" => host = Some(value.trim().to_owned()),
            "port" => port = value.trim().parse().unwrap_or(22),
            "hostkeyalias" => alias = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    let host = host?;
    let name = alias.unwrap_or_else(|| host.clone());
    let known_hosts_name = if port == 22 {
        name
    } else {
        format!("[{name}]:{port}")
    };
    Some(Resolved {
        host,
        port,
        known_hosts_name,
    })
}

fn run(command: &mut Command, input: Option<&str>) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin
            .write_all(input.as_bytes())
            .map_err(|error| error.to_string())?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(last_line(&String::from_utf8_lossy(&output.stderr)).to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Scans `target`'s host keys the way SSH will see them.
pub(crate) fn scan(target: &str) -> Result<ScannedKey, String> {
    let config = run(Command::new("ssh").arg("-G").arg(target), None)?;
    let resolved = resolved_from_config(&config).ok_or("ssh -G gave no hostname")?;
    let scanned = run(
        Command::new("ssh-keyscan")
            .args(["-T", "10", "-p"])
            .arg(resolved.port.to_string())
            .arg(&resolved.host),
        None,
    )?;
    let entries = known_hosts_entries(&scanned, &resolved.known_hosts_name);
    if entries.is_empty() {
        return Err(format!("{} did not return a host key", resolved.host));
    }
    let listed = run(
        Command::new("ssh-keygen").args(["-l", "-f", "-"]),
        Some(&entries.join("\n")),
    )?;
    Ok(ScannedKey {
        host: resolved.known_hosts_name,
        fingerprints: fingerprints(&listed),
        entries,
    })
}

/// `known_hosts` lines naming `name`, from `ssh-keyscan` output.
fn known_hosts_entries(scanned: &str, name: &str) -> Vec<String> {
    scanned
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _scanned_host = fields.next()?;
            let key_type = fields.next()?;
            let key = fields.next()?;
            Some(format!("{name} {key_type} {key}"))
        })
        .collect()
}

/// `ED25519 SHA256:…` from `ssh-keygen -l` lines like `256 SHA256:… host (ED25519)`.
fn fingerprints(listed: &str) -> Vec<String> {
    listed
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _bits = fields.next()?;
            let hash = fields.next()?;
            let kind = line
                .rsplit_once('(')
                .map(|(_, kind)| kind.trim_end_matches(')'))
                .unwrap_or("key");
            Some(format!("{kind} {hash}"))
        })
        .collect()
}

fn known_hosts_path() -> Option<PathBuf> {
    // Windows OpenSSH reads %USERPROFILE%\.ssh; a Git Bash HOME may not match it.
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    }?;
    Some(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

/// Records the scanned keys so later connections verify against them.
pub(crate) fn trust(key: &ScannedKey) -> Result<(), String> {
    let path = known_hosts_path().ok_or("can't find your home folder")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("can't write {}: {e}", path.display()))?;
    let needs_newline = std::fs::read(&path)
        .map(|bytes| bytes.last().is_some_and(|last| *last != b'\n'))
        .unwrap_or(false);
    let mut text = if needs_newline {
        "\n".to_owned()
    } else {
        String::new()
    };
    for entry in &key.entries {
        text.push_str(entry);
        text.push('\n');
    }
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ssh_errors() {
        let unknown = "remote platform detection failed: No ED25519 host key is known for 100.127.93.17 and you have requested strict checking.\r\nHost key verification failed.";
        assert_eq!(classify(unknown), SshProblem::UnknownHostKey);
        let changed = "@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nHost key verification failed.";
        assert_eq!(classify(changed), SshProblem::ChangedHostKey);
        assert_eq!(
            classify("ssh: Could not resolve hostname linuxhome-ts: Name or service not known"),
            SshProblem::UnknownHost
        );
        assert_eq!(
            classify("bonal@host: Permission denied (publickey)."),
            SshProblem::AuthFailed
        );
        assert_eq!(
            classify("ssh: connect to host x port 22: Connection timed out"),
            SshProblem::Unreachable
        );
    }

    #[test]
    fn ssh_config_resolves_host_port_and_alias() {
        let config = "user bonal\nhostname 100.127.93.17\nport 22\n";
        let resolved = resolved_from_config(config).unwrap();
        assert_eq!(resolved.host, "100.127.93.17");
        assert_eq!(resolved.known_hosts_name, "100.127.93.17");
        let config = "hostname box.example\nport 2222\nhostkeyalias mybox\n";
        let resolved = resolved_from_config(config).unwrap();
        assert_eq!(resolved.port, 2222);
        assert_eq!(resolved.known_hosts_name, "[mybox]:2222");
    }

    #[test]
    fn scanned_keys_become_known_hosts_lines_for_the_resolved_name() {
        let scanned =
            "# 100.127.93.17:22 SSH-2.0-OpenSSH_9.6\n100.127.93.17 ssh-ed25519 AAAAC3Nza\n";
        assert_eq!(
            known_hosts_entries(scanned, "[box]:2222"),
            ["[box]:2222 ssh-ed25519 AAAAC3Nza"]
        );
        assert_eq!(
            fingerprints("256 SHA256:patyt [box]:2222 (ED25519)\n"),
            ["ED25519 SHA256:patyt"]
        );
    }
}
