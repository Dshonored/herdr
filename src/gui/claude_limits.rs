//! Claude Code plan limits (5-hour and weekly used percentage).
//!
//! Claude Code passes `rate_limits.five_hour` / `rate_limits.seven_day` to its status line
//! command (Pro and Max plans; <https://code.claude.com/docs/en/statusline>). When the user
//! enables it, the `statusLine` setting is wrapped so each status line update is also saved
//! to `~/.claude/herdr-usage.json`, then forwarded unchanged to the user's own status line
//! command. The original setting is kept in `~/.claude/herdr-usage-statusline.json` and
//! restored exactly on disable.

use std::io::{self, Write as _};
use std::path::PathBuf;

use jsonc_parser::cst::{CstInputValue, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::Value;

use super::usage::{LimitWindow, PlanLimits};

pub(crate) const CAPTURE_FILE: &str = "herdr-usage.json";
const BACKUP_FILE: &str = "herdr-usage-statusline.json";
const MARKER: &str = "# herdr-usage";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LimitsHook {
    Checking,
    Enabled,
    Disabled,
    Failed(String),
}

/// Status line command that saves its input and forwards it to `original`.
pub(crate) fn wrapped_command(original: Option<&str>) -> String {
    let forward = original
        .filter(|command| !command.trim().is_empty())
        .map(|command| format!("printf '%s' \"$input\" | {{\n{command}\n}}\n"))
        .unwrap_or_default();
    format!(
        "{MARKER}\ninput=$(cat)\nprintf '%s' \"$input\" > \"$HOME/.claude/{CAPTURE_FILE}\" 2>/dev/null\n{forward}"
    )
}

fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn input_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input_value).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), input_value(value)))
                .collect(),
        ),
    }
}

fn root(settings: &str) -> Result<CstRootNode, String> {
    let text = if settings.trim().is_empty() {
        "{}\n"
    } else {
        settings
    };
    let root = CstRootNode::parse(text, &parse_options())
        .map_err(|error| format!("~/.claude/settings.json is not valid JSON: {error}"))?;
    if root.value().and_then(|value| value.as_object()).is_none() {
        return Err("~/.claude/settings.json must contain a JSON object".into());
    }
    Ok(root)
}

fn status_line(settings: &str) -> Result<Option<Value>, String> {
    let root = root(settings)?;
    Ok(root
        .value()
        .and_then(|value| value.as_object())
        .and_then(|object| object.get("statusLine"))
        .and_then(|property| property.to_serde_value()))
}

pub(crate) fn is_installed(settings: &str) -> Result<bool, String> {
    Ok(status_line(settings)?
        .as_ref()
        .and_then(|line| line.get("command"))
        .and_then(Value::as_str)
        .is_some_and(|command| command.starts_with(MARKER)))
}

/// Returns the new settings text and the original `statusLine` value to back up.
pub(crate) fn install(settings: &str) -> Result<(String, Value), String> {
    if is_installed(settings)? {
        return Err("already enabled".into());
    }
    let original = status_line(settings)?.unwrap_or(Value::Null);
    let original_command = original.get("command").and_then(Value::as_str);
    let mut wrapped = match &original {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    wrapped.insert("type".into(), Value::String("command".into()));
    wrapped.insert(
        "command".into(),
        Value::String(wrapped_command(original_command)),
    );
    let root = root(settings)?;
    let object = root
        .value()
        .and_then(|value| value.as_object())
        .ok_or("~/.claude/settings.json must contain a JSON object")?;
    let replacement = input_value(&Value::Object(wrapped));
    match object.get("statusLine") {
        Some(property) => property.set_value(replacement),
        None => {
            object.append("statusLine", replacement);
        }
    }
    Ok((root.to_string(), original))
}

/// Restores the backed-up `statusLine` (or removes it when there was none).
pub(crate) fn uninstall(settings: &str, original: &Value) -> Result<String, String> {
    if !is_installed(settings)? {
        return Ok(settings.to_owned());
    }
    let root = root(settings)?;
    let object = root
        .value()
        .and_then(|value| value.as_object())
        .ok_or("~/.claude/settings.json must contain a JSON object")?;
    if let Some(property) = object.get("statusLine") {
        if original.is_null() {
            property.remove();
        } else {
            property.set_value(input_value(original));
        }
    }
    Ok(root.to_string())
}

/// What the latest saved status line input says about the Claude Code that wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CaptureInfo {
    pub(crate) version: Option<String>,
    pub(crate) at: i64,
}

pub(crate) fn capture_info(text: &str, captured_at: i64) -> Option<CaptureInfo> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    Some(CaptureInfo {
        version: value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        at: captured_at,
    })
}

/// Reads plan limits from a saved status line input.
pub(crate) fn parse_capture(text: &str, captured_at: i64, now: i64) -> Option<PlanLimits> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    let limits = value.get("rate_limits")?;
    let windows: Vec<LimitWindow> = [("five_hour", 300), ("seven_day", 10080)]
        .into_iter()
        .filter_map(|(key, window_minutes)| {
            let window = limits.get(key)?;
            let resets_at = window.get("resets_at").and_then(Value::as_i64);
            // A window past its reset time no longer describes current usage.
            if resets_at.is_some_and(|at| at <= now) {
                return None;
            }
            Some(LimitWindow {
                window_minutes,
                used_percent: window.get("used_percentage")?.as_f64()?,
                resets_at,
            })
        })
        .collect();
    (!windows.is_empty()).then_some(PlanLimits {
        at: captured_at,
        plan: None,
        windows,
    })
}

// ----- this computer -------------------------------------------------------------

fn claude_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".claude"))
}

pub(crate) fn local_capture_path() -> Option<PathBuf> {
    claude_dir().map(|dir| dir.join(CAPTURE_FILE))
}

fn read_or_empty(path: &std::path::Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

/// Writes through a temporary file so a crash never leaves a truncated settings file.
fn write_atomic(path: &std::path::Path, text: &str) -> io::Result<()> {
    let temporary = path.with_extension("herdr-tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

pub(crate) fn local_status() -> Result<bool, String> {
    let dir = claude_dir().ok_or("no home directory")?;
    let settings = read_or_empty(&dir.join("settings.json")).map_err(|e| e.to_string())?;
    is_installed(&settings)
}

pub(crate) fn local_set(enabled: bool) -> Result<(), String> {
    let dir = claude_dir().ok_or("no home directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let settings_path = dir.join("settings.json");
    let backup_path = dir.join(BACKUP_FILE);
    let settings = read_or_empty(&settings_path).map_err(|e| e.to_string())?;
    if enabled {
        let (updated, original) = install(&settings)?;
        let backup = serde_json::json!({ "original": original }).to_string();
        std::fs::write(&backup_path, backup).map_err(|e| e.to_string())?;
        write_atomic(&settings_path, &updated).map_err(|e| e.to_string())
    } else {
        let original = read_backup(&read_or_empty(&backup_path).map_err(|e| e.to_string())?);
        let restored = uninstall(&settings, &original)?;
        write_atomic(&settings_path, &restored).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&backup_path);
        Ok(())
    }
}

fn read_backup(text: &str) -> Value {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("original").cloned())
        .unwrap_or(Value::Null)
}

// ----- SSH machines ----------------------------------------------------------------

const SETTINGS_SEPARATOR: &str = "\n@@HERDR_BACKUP@@\n";

fn ssh(target: &str, script: &str, input: Option<&str>) -> Result<String, String> {
    let mut child = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            target,
            script,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run ssh: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(input) = input {
            stdin
                .write_all(input.as_bytes())
                .map_err(|error| error.to_string())?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh {target} failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn remote_read(target: &str) -> Result<(String, String), String> {
    let output = ssh(
        target,
        &format!(
            "cat ~/.claude/settings.json 2>/dev/null; printf '{}'; cat ~/.claude/{BACKUP_FILE} 2>/dev/null; exit 0",
            SETTINGS_SEPARATOR.replace('\n', "\\n")
        ),
        None,
    )?;
    let (settings, backup) = output
        .rsplit_once(SETTINGS_SEPARATOR)
        .ok_or("unexpected output while reading ~/.claude/settings.json")?;
    Ok((settings.to_owned(), backup.to_owned()))
}

fn remote_write(target: &str, file: &str, text: &str) -> Result<(), String> {
    ssh(
        target,
        &format!("mkdir -p ~/.claude && cat > ~/.claude/.{file}.herdr-tmp && mv ~/.claude/.{file}.herdr-tmp ~/.claude/{file}"),
        Some(text),
    )
    .map(|_| ())
}

pub(crate) fn remote_status(target: &str) -> Result<bool, String> {
    is_installed(&remote_read(target)?.0)
}

pub(crate) fn remote_set(target: &str, enabled: bool) -> Result<(), String> {
    let (settings, backup) = remote_read(target)?;
    if enabled {
        let (updated, original) = install(&settings)?;
        remote_write(
            target,
            BACKUP_FILE,
            &serde_json::json!({ "original": original }).to_string(),
        )?;
        remote_write(target, "settings.json", &updated)
    } else {
        let restored = uninstall(&settings, &read_backup(&backup))?;
        remote_write(target, "settings.json", &restored)?;
        ssh(target, &format!("rm -f ~/.claude/{BACKUP_FILE}"), None).map(|_| ())
    }
}

/// Lines appended to the remote usage script: `@@CLAUDE_LIMITS <mtime>` then the capture.
pub(crate) fn remote_script_tail() -> String {
    format!(
        r#"f="$HOME/.claude/{CAPTURE_FILE}"
if [ -f "$f" ]; then
  printf '@@CLAUDE_LIMITS %s\n' "$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo 0)"
  cat "$f"
  printf '\n'
fi
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: &str = r#"{
  // user comment stays
  "model": "opus",
  "statusLine": {
    "type": "command",
    "command": "bash \"/home/u/.claude/statusline.sh\"",
    "refreshInterval": 1
  },
  "hooks": {}
}
"#;

    #[test]
    fn install_wraps_and_uninstall_restores_exactly() {
        let (installed, original) = install(SETTINGS).unwrap();
        assert!(is_installed(&installed).unwrap());
        assert!(installed.contains("// user comment stays"));
        assert!(installed.contains(r#""model": "opus""#));
        let line = status_line(&installed).unwrap().unwrap();
        let command = line["command"].as_str().unwrap();
        assert!(command.starts_with(MARKER));
        assert!(command.contains("herdr-usage.json"));
        assert!(command.contains("bash \"/home/u/.claude/statusline.sh\""));
        assert_eq!(line["refreshInterval"], 1);
        assert_eq!(install(&installed).unwrap_err(), "already enabled");

        let restored = uninstall(&installed, &original).unwrap();
        assert_eq!(
            status_line(&restored).unwrap(),
            status_line(SETTINGS).unwrap()
        );
        assert!(!is_installed(&restored).unwrap());
    }

    #[test]
    fn install_without_a_status_line_adds_and_removes_one() {
        let (installed, original) = install("").unwrap();
        assert_eq!(original, Value::Null);
        let command = status_line(&installed).unwrap().unwrap()["command"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!command.contains('|'), "nothing to forward to: {command}");
        let restored = uninstall(&installed, &original).unwrap();
        assert_eq!(status_line(&restored).unwrap(), None);
    }

    #[test]
    fn capture_reads_documented_rate_limits_and_drops_reset_windows() {
        let text = r#"{"model":{"id":"x"},"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":2000},"seven_day":{"used_percentage":41.2,"resets_at":900}}}"#;
        let limits = parse_capture(text, 800, 1000).unwrap();
        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].label(), "5-hour");
        assert_eq!(limits.windows[0].used_percent, 23.5);
        assert_eq!(limits.windows[0].resets_at, Some(2000));
        assert!(parse_capture(r#"{"model":{}}"#, 0, 0).is_none());
    }

    #[test]
    fn wrapped_command_forwards_input_to_the_original() {
        let command = wrapped_command(Some("my-statusline --fast"));
        assert!(command.contains("input=$(cat)"));
        assert!(command.ends_with("printf '%s' \"$input\" | {\nmy-statusline --fast\n}\n"));
    }
}
