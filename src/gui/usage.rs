//! Coding-agent usage from the agents' own session logs.
//!
//! Supported sources (newest 8 days of logs):
//! - Claude Code: `~/.claude/projects/**/*.jsonl` (and `~/.config/claude/projects`), token
//!   counts per API response.
//! - Codex: `~/.codex/sessions/**/*.jsonl`, token counts plus the plan's rate-limit windows.
//! - pi: `~/.pi/agent/sessions/**/*.jsonl`, token counts and cost.
//!
//! Local logs are read directly. Remote machines run a POSIX `find`/`grep` script over SSH
//! that returns only the usage fields, never conversation text; both paths feed the same
//! field extraction below.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;

/// How far back logs are read; covers the weekly window.
const SCAN_DAYS: u64 = 8;
const HOUR: i64 = 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum AgentKind {
    ClaudeCode,
    Codex,
    Pi,
}

impl AgentKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Pi => "pi",
        }
    }

    fn from_path(path: &str) -> Option<Self> {
        let path = path.replace('\\', "/");
        if path.contains("/.codex/sessions/") {
            Some(Self::Codex)
        } else if path.contains("/.pi/agent/sessions/") {
            Some(Self::Pi)
        } else if path.contains("/.claude/projects/") || path.contains("/claude/projects/") {
            Some(Self::ClaudeCode)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Totals {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) requests: u64,
}

impl Totals {
    pub(crate) fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn add(&mut self, event: &UsageEvent) {
        self.input += event.input;
        self.output += event.output;
        self.cache_read += event.cache_read;
        self.cache_write += event.cache_write;
        if let Some(cost) = event.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost;
        }
        self.requests += 1;
    }
}

#[derive(Clone, Debug, PartialEq)]
struct UsageEvent {
    agent: AgentKind,
    at: i64,
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost_usd: Option<f64>,
    /// Identity of one API response, for logs that repeat it.
    dedup: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LimitWindow {
    pub(crate) window_minutes: u64,
    pub(crate) used_percent: f64,
    pub(crate) resets_at: Option<i64>,
}

impl LimitWindow {
    pub(crate) fn label(&self) -> String {
        match self.window_minutes {
            300 => "5-hour".into(),
            10080 => "Weekly".into(),
            minutes if minutes % 1440 == 0 => format!("{}d", minutes / 1440),
            minutes if minutes % 60 == 0 => format!("{}h", minutes / 60),
            minutes => format!("{minutes}m"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlanLimits {
    pub(crate) at: i64,
    pub(crate) plan: Option<String>,
    pub(crate) windows: Vec<LimitWindow>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AgentUsage {
    pub(crate) last_5h: Totals,
    pub(crate) last_24h: Totals,
    pub(crate) last_7d: Totals,
    /// Seven-day totals by model, largest first.
    pub(crate) models: Vec<(String, Totals)>,
    pub(crate) limits: Option<PlanLimits>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct UsageReport {
    pub(crate) agents: Vec<(AgentKind, AgentUsage)>,
    pub(crate) generated_at: i64,
}

// ----- field extraction ------------------------------------------------------

static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| re(r#""timestamp":\s*"([^"]+)""#));
static MODEL: LazyLock<Regex> = LazyLock::new(|| re(r#""model":\s*"([^"]+)""#));
static REQUEST_ID: LazyLock<Regex> = LazyLock::new(|| re(r#""requestId":\s*"([^"]+)""#));
static MESSAGE_ID: LazyLock<Regex> = LazyLock::new(|| re(r#""id":\s*"(msg_[^"]+)""#));
/// The flat start of a usage object, up to its first nested object.
static USAGE: LazyLock<Regex> = LazyLock::new(|| re(r#""usage":\s*\{([^{}]*)"#));
static COST: LazyLock<Regex> = LazyLock::new(|| re(r#""cost":\s*\{([^{}]*)\}"#));
static LAST_USAGE: LazyLock<Regex> = LazyLock::new(|| re(r#""last_token_usage":\s*\{([^{}]*)\}"#));
static TOTAL_USAGE: LazyLock<Regex> =
    LazyLock::new(|| re(r#""total_token_usage":\s*\{([^{}]*)\}"#));
static LIMIT_WINDOW: LazyLock<Regex> =
    LazyLock::new(|| re(r#""(?:primary|secondary)":\s*\{([^{}]*)\}"#));
static PLAN_TYPE: LazyLock<Regex> = LazyLock::new(|| re(r#""plan_type":\s*"([^"]+)""#));
static EVENT_TYPE: LazyLock<Regex> =
    LazyLock::new(|| re(r#""type":\s*"(turn_context|token_count)""#));

fn re(pattern: &str) -> Regex {
    // Patterns are constants covered by tests.
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid usage pattern {pattern}: {error}"))
}

/// Grep alternation sent to remote machines; it must capture every pattern above.
/// Character classes are written `[^}{]` because `find -exec` rewrites any literal `{}`.
const REMOTE_PATTERNS: &[&str] = &[
    r#""timestamp": ?"[^"]*""#,
    r#""model": ?"[^"]*""#,
    r#""requestId": ?"[^"]*""#,
    r#""id": ?"msg_[^"]*""#,
    // Whole `key: value,` members only, so a following `"cost":{` stays matchable
    // (grep -o never reports overlapping matches).
    r#""usage": ?\{([^}{,]*,)*"#,
    r#""cost": ?\{[^}{]*\}"#,
    r#""last_token_usage": ?\{[^}{]*\}"#,
    r#""total_token_usage": ?\{[^}{]*\}"#,
    r#""primary": ?\{[^}{]*\}"#,
    r#""secondary": ?\{[^}{]*\}"#,
    r#""plan_type": ?"[^"]*""#,
    r#""type": ?"(turn_context|token_count)""#,
];

fn capture<'a>(pattern: &Regex, text: &'a str) -> Option<&'a str> {
    pattern
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str())
}

/// A numeric field inside a flat JSON fragment.
fn number(fragment: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\":");
    let start = fragment.find(&needle)? + needle.len();
    let rest = fragment[start..].trim_start();
    let end = rest
        .find(|ch: char| !(ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn count(fragment: &str, key: &str) -> u64 {
    number(fragment, key).map_or(0, |value| value.max(0.0) as u64)
}

fn parse_time(value: &str) -> Option<i64> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

/// Per-file parsing state; Codex attributes tokens to the latest `turn_context` model.
#[derive(Default)]
struct FileState {
    model: Option<String>,
    last_total: Option<u64>,
}

enum Parsed {
    Event(UsageEvent),
    Limits(AgentKind, PlanLimits),
}

fn parse_line(agent: AgentKind, line: &str, state: &mut FileState) -> Vec<Parsed> {
    match agent {
        AgentKind::ClaudeCode => parse_claude(line).map(Parsed::Event).into_iter().collect(),
        AgentKind::Pi => parse_pi(line).map(Parsed::Event).into_iter().collect(),
        AgentKind::Codex => parse_codex(line, state),
    }
}

fn parse_claude(line: &str) -> Option<UsageEvent> {
    let usage = capture(&USAGE, line)?;
    let model = capture(&MODEL, line)?;
    if model.starts_with('<') {
        // `<synthetic>` entries are local placeholders, not API responses.
        return None;
    }
    let dedup = match (capture(&MESSAGE_ID, line), capture(&REQUEST_ID, line)) {
        (Some(message), Some(request)) => Some(format!("{message}:{request}")),
        (Some(message), None) => Some(message.to_owned()),
        _ => None,
    };
    Some(UsageEvent {
        agent: AgentKind::ClaudeCode,
        at: parse_time(capture(&TIMESTAMP, line)?)?,
        model: model.to_owned(),
        input: count(usage, "input_tokens"),
        output: count(usage, "output_tokens"),
        cache_read: count(usage, "cache_read_input_tokens"),
        cache_write: count(usage, "cache_creation_input_tokens"),
        cost_usd: None,
        dedup,
    })
}

fn parse_pi(line: &str) -> Option<UsageEvent> {
    let usage = capture(&USAGE, line)?;
    if !usage.contains("\"input\"") {
        return None;
    }
    Some(UsageEvent {
        agent: AgentKind::Pi,
        at: parse_time(capture(&TIMESTAMP, line)?)?,
        model: capture(&MODEL, line).unwrap_or("unknown").to_owned(),
        input: count(usage, "input"),
        output: count(usage, "output"),
        cache_read: count(usage, "cacheRead"),
        cache_write: count(usage, "cacheWrite"),
        cost_usd: capture(&COST, line).and_then(|cost| number(cost, "total")),
        dedup: None,
    })
}

fn parse_codex(line: &str, state: &mut FileState) -> Vec<Parsed> {
    let mut parsed = Vec::new();
    match capture(&EVENT_TYPE, line) {
        Some("turn_context") => {
            if let Some(model) = capture(&MODEL, line) {
                state.model = Some(model.to_owned());
            }
            return parsed;
        }
        Some("token_count") => {}
        _ => return parsed,
    }
    let Some(at) = capture(&TIMESTAMP, line).and_then(parse_time) else {
        return parsed;
    };

    // Codex can repeat a token_count; only count it when the running total moved.
    let total = capture(&TOTAL_USAGE, line).map(|total| count(total, "total_tokens"));
    let repeated = total.is_some() && total == state.last_total;
    if let (Some(usage), false) = (capture(&LAST_USAGE, line), repeated) {
        state.last_total = total;
        let input = count(usage, "input_tokens");
        let cached = count(usage, "cached_input_tokens").min(input);
        parsed.push(Parsed::Event(UsageEvent {
            agent: AgentKind::Codex,
            at,
            model: state.model.clone().unwrap_or_else(|| "unknown".into()),
            input: input - cached,
            output: count(usage, "output_tokens"),
            cache_read: cached,
            cache_write: 0,
            cost_usd: None,
            dedup: None,
        }));
    }

    let windows: Vec<LimitWindow> = LIMIT_WINDOW
        .captures_iter(line)
        .filter_map(|captures| {
            let window = captures.get(1)?.as_str();
            let resets_at = number(window, "resets_at")
                .map(|value| value as i64)
                .or_else(|| number(window, "resets_in_seconds").map(|secs| at + secs as i64));
            Some(LimitWindow {
                window_minutes: number(window, "window_minutes")? as u64,
                used_percent: number(window, "used_percent")?,
                resets_at,
            })
        })
        .collect();
    if !windows.is_empty() {
        parsed.push(Parsed::Limits(
            AgentKind::Codex,
            PlanLimits {
                at,
                plan: capture(&PLAN_TYPE, line).map(str::to_owned),
                windows,
            },
        ));
    }
    parsed
}

// ----- aggregation -----------------------------------------------------------

#[derive(Default)]
struct Collected {
    events: Vec<UsageEvent>,
    limits: HashMap<AgentKind, PlanLimits>,
}

impl Collected {
    fn push(&mut self, parsed: Parsed) {
        match parsed {
            Parsed::Event(event) => self.events.push(event),
            Parsed::Limits(agent, limits) => {
                let newer = self
                    .limits
                    .get(&agent)
                    .is_none_or(|current| limits.at >= current.at);
                if newer {
                    self.limits.insert(agent, limits);
                }
            }
        }
    }

    fn report(self, now: i64) -> UsageReport {
        let mut seen = HashSet::new();
        let mut agents: HashMap<AgentKind, AgentUsage> = HashMap::new();
        let mut models: HashMap<(AgentKind, String), Totals> = HashMap::new();
        for event in &self.events {
            if let Some(key) = &event.dedup {
                if !seen.insert((event.agent, key.clone())) {
                    continue;
                }
            }
            let age = now - event.at;
            if !(0..7 * 24 * HOUR).contains(&age) {
                continue;
            }
            let usage = agents.entry(event.agent).or_default();
            usage.last_7d.add(event);
            if age < 24 * HOUR {
                usage.last_24h.add(event);
            }
            if age < 5 * HOUR {
                usage.last_5h.add(event);
            }
            models
                .entry((event.agent, event.model.clone()))
                .or_default()
                .add(event);
        }
        for (agent, limits) in self.limits {
            agents.entry(agent).or_default().limits = Some(limits);
        }
        for ((agent, model), totals) in models {
            if let Some(usage) = agents.get_mut(&agent) {
                usage.models.push((model, totals));
            }
        }
        let mut agents: Vec<(AgentKind, AgentUsage)> = agents.into_iter().collect();
        for (_, usage) in &mut agents {
            usage
                .models
                .sort_by_key(|(_, totals)| std::cmp::Reverse(totals.tokens()));
        }
        agents.sort_by_key(|(agent, _)| *agent);
        UsageReport {
            agents,
            generated_at: now,
        }
    }
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

// ----- local scan --------------------------------------------------------------

#[derive(Clone)]
struct CachedFile {
    modified: SystemTime,
    len: u64,
    parsed: Vec<ParsedOwned>,
}

/// `Parsed` without borrowing, so files can be cached between scans.
#[derive(Clone)]
enum ParsedOwned {
    Event(UsageEvent),
    Limits(AgentKind, PlanLimits),
}

impl From<Parsed> for ParsedOwned {
    fn from(parsed: Parsed) -> Self {
        match parsed {
            Parsed::Event(event) => Self::Event(event),
            Parsed::Limits(agent, limits) => Self::Limits(agent, limits),
        }
    }
}

impl From<ParsedOwned> for Parsed {
    fn from(parsed: ParsedOwned) -> Self {
        match parsed {
            ParsedOwned::Event(event) => Self::Event(event),
            ParsedOwned::Limits(agent, limits) => Self::Limits(agent, limits),
        }
    }
}

/// Re-reads only log files that changed since the previous scan.
#[derive(Default)]
pub(crate) struct LocalScanner {
    files: HashMap<PathBuf, CachedFile>,
}

fn local_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    vec![
        home.join(".claude").join("projects"),
        home.join(".config").join("claude").join("projects"),
        home.join(".codex").join("sessions"),
        home.join(".pi").join("agent").join("sessions"),
    ]
}

fn collect_jsonl(dir: &Path, since: SystemTime, out: &mut Vec<(PathBuf, std::fs::Metadata)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let path = entry.path();
        if metadata.is_dir() {
            collect_jsonl(&path, since, out);
        } else if path.extension().is_some_and(|ext| ext == "jsonl")
            && metadata.modified().is_ok_and(|modified| modified >= since)
        {
            out.push((path, metadata));
        }
    }
}

fn wanted_line(agent: AgentKind, line: &str) -> bool {
    match agent {
        AgentKind::ClaudeCode | AgentKind::Pi => line.contains("\"usage\""),
        AgentKind::Codex => line.contains("token_count") || line.contains("turn_context"),
    }
}

impl LocalScanner {
    pub(crate) fn scan(&mut self) -> UsageReport {
        let since = SystemTime::now() - Duration::from_secs(SCAN_DAYS * 24 * 3600);
        let mut files = Vec::new();
        for root in local_roots() {
            collect_jsonl(&root, since, &mut files);
        }
        let mut collected = Collected::default();
        let mut live = HashSet::new();
        for (path, metadata) in files {
            let Some(agent) = AgentKind::from_path(&path.to_string_lossy()) else {
                continue;
            };
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            let fresh = self
                .files
                .get(&path)
                .is_some_and(|cached| cached.modified == modified && cached.len == metadata.len());
            if !fresh {
                let parsed = parse_file(agent, &path).unwrap_or_default();
                self.files.insert(
                    path.clone(),
                    CachedFile {
                        modified,
                        len: metadata.len(),
                        parsed,
                    },
                );
            }
            if let Some(cached) = self.files.get(&path) {
                for parsed in &cached.parsed {
                    collected.push(parsed.clone().into());
                }
            }
            live.insert(path);
        }
        self.files.retain(|path, _| live.contains(path));
        let now = now();
        if let Some(limits) = local_claude_limits(now) {
            collected.push(Parsed::Limits(AgentKind::ClaudeCode, limits));
        }
        collected.report(now)
    }
}

fn local_claude_limits(now: i64) -> Option<PlanLimits> {
    let path = super::claude_limits::local_capture_path()?;
    let captured_at = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let text = std::fs::read_to_string(&path).ok()?;
    super::claude_limits::parse_capture(&text, captured_at, now)
}

fn parse_file(agent: AgentKind, path: &Path) -> io::Result<Vec<ParsedOwned>> {
    let reader = io::BufReader::new(std::fs::File::open(path)?);
    let mut state = FileState::default();
    let mut parsed = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        if wanted_line(agent, &line) {
            parsed.extend(
                parse_line(agent, &line, &mut state)
                    .into_iter()
                    .map(Into::into),
            );
        }
    }
    Ok(parsed)
}

// ----- remote scan -------------------------------------------------------------

/// POSIX script run on a remote machine. Prints `@@FILE <path>` and then, per matching log
/// line, `<line>:` followed by one usage field per output line.
pub(crate) fn remote_script() -> String {
    // The patterns sit inside two levels of single quotes; none may contain one.
    let pattern = REMOTE_PATTERNS.join("|");
    format!(
        r#"set -f
for root in "$HOME/.claude/projects" "$HOME/.config/claude/projects" "$HOME/.codex/sessions" "$HOME/.pi/agent/sessions"; do
  [ -d "$root" ] || continue
  find "$root" -type f -name '*.jsonl' -mtime -{days} -exec sh -c '
    for f; do
      printf "@@FILE %s\n" "$f"
      grep -nE "\"usage\"|token_count|turn_context" "$f" | grep -oE '"'"'^[0-9]+:|{pattern}'"'"'
    done' sh {{}} +
done 2>/dev/null
{claude_limits}exit 0
"#,
        days = SCAN_DAYS,
        claude_limits = super::claude_limits::remote_script_tail(),
    )
}

/// Rebuilds compact log lines from the remote script's output and parses them.
pub(crate) fn parse_remote_output(output: &str, now: i64) -> UsageReport {
    let mut collected = Collected::default();
    let mut agent = None;
    let mut state = FileState::default();
    let mut line = String::new();
    let mut flush = |agent: Option<AgentKind>, line: &mut String, state: &mut FileState| {
        if let Some(agent) = agent {
            if !line.is_empty() {
                for parsed in parse_line(agent, line, state) {
                    collected.push(parsed);
                }
            }
        }
        line.clear();
    };
    // Everything after this marker is Claude Code's saved status line input.
    let mut claude_capture: Option<(i64, String)> = None;
    for raw in output.lines() {
        if let Some((_, capture)) = claude_capture.as_mut() {
            capture.push_str(raw);
            capture.push('\n');
        } else if let Some(mtime) = raw.strip_prefix("@@CLAUDE_LIMITS ") {
            flush(agent, &mut line, &mut state);
            agent = None;
            claude_capture = Some((mtime.trim().parse().unwrap_or(now), String::new()));
        } else if let Some(path) = raw.strip_prefix("@@FILE ") {
            flush(agent, &mut line, &mut state);
            agent = AgentKind::from_path(path);
            state = FileState::default();
        } else if raw.ends_with(':') && raw[..raw.len() - 1].bytes().all(|b| b.is_ascii_digit()) {
            flush(agent, &mut line, &mut state);
        } else {
            // A cut-off `"usage":{...` fragment still parses as a flat prefix.
            line.push_str(raw);
            line.push(',');
        }
    }
    flush(agent, &mut line, &mut state);
    if let Some(limits) =
        claude_capture.and_then(|(at, text)| super::claude_limits::parse_capture(&text, at, now))
    {
        collected.push(Parsed::Limits(AgentKind::ClaudeCode, limits));
    }
    collected.report(now)
}

/// Runs the usage script on a saved SSH machine.
pub(crate) fn scan_remote(target: &str) -> io::Result<UsageReport> {
    let mut child = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-C",
            target,
            "sh -s",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(remote_script().as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "usage scan on {target} failed ({})",
            output.status
        )));
    }
    Ok(parse_remote_output(
        &String::from_utf8_lossy(&output.stdout),
        now(),
    ))
}

/// Compact token count, e.g. `950`, `12.3K`, `4.5M`, `1.2B`.
pub(crate) fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}K", tokens as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", tokens as f64 / 1e6),
        _ => format!("{:.1}B", tokens as f64 / 1e9),
    }
}

/// Time until `at`, e.g. `2h 10m` or `3d 4h`.
pub(crate) fn format_until(at: i64, now: i64) -> String {
    let seconds = (at - now).max(0);
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_590_400; // 2026-10-10T00:00:00Z

    fn iso(offset_hours: i64) -> String {
        time::OffsetDateTime::from_unix_timestamp(NOW - offset_hours * HOUR)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    fn claude_line(hours_ago: i64, id: &str) -> String {
        format!(
            r#"{{"parentUuid":"x","message":{{"id":"{id}","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{{"type":"text","text":"say \"model\":\"fake\""}}],"usage":{{"input_tokens":10,"cache_creation_input_tokens":200,"cache_read_input_tokens":3000,"output_tokens":40,"output_tokens_details":{{"thinking_tokens":5}},"iterations":[{{"input_tokens":999}}]}}}},"requestId":"req_1","type":"assistant","timestamp":"{}"}}"#,
            iso(hours_ago)
        )
    }

    fn report_from_lines(agent: AgentKind, lines: &[String]) -> UsageReport {
        let mut collected = Collected::default();
        let mut state = FileState::default();
        for line in lines {
            if wanted_line(agent, line) {
                for parsed in parse_line(agent, line, &mut state) {
                    collected.push(parsed);
                }
            }
        }
        collected.report(NOW)
    }

    #[test]
    fn claude_usage_is_windowed_and_deduplicated() {
        let report = report_from_lines(
            AgentKind::ClaudeCode,
            &[
                claude_line(1, "msg_a"),
                claude_line(1, "msg_a"),
                claude_line(10, "msg_b"),
                claude_line(100, "msg_c"),
                claude_line(200, "msg_old"),
            ],
        );
        let (agent, usage) = &report.agents[0];
        assert_eq!(*agent, AgentKind::ClaudeCode);
        assert_eq!(usage.last_5h.requests, 1);
        assert_eq!(usage.last_24h.requests, 2);
        assert_eq!(usage.last_7d.requests, 3);
        assert_eq!(usage.last_5h.input, 10);
        assert_eq!(usage.last_5h.output, 40);
        assert_eq!(usage.last_5h.cache_read, 3000);
        assert_eq!(usage.last_5h.cache_write, 200);
        assert_eq!(usage.models[0].0, "claude-opus-5-5");
    }

    #[test]
    fn codex_counts_new_turns_and_keeps_latest_limits() {
        let context = r#"{"timestamp":"2026-10-09T23:00:00Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#;
        let token = |at: &str, total: u64, used: f64| {
            format!(
                r#"{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1,"total_tokens":{total}}},"last_token_usage":{{"input_tokens":100,"cached_input_tokens":60,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120}}}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used},"window_minutes":300,"resets_at":1791600000}},"secondary":{{"used_percent":12.0,"window_minutes":10080,"resets_at":1792000000}},"plan_type":"prolite"}}}}}}"#
            )
        };
        let report = report_from_lines(
            AgentKind::Codex,
            &[
                context.to_owned(),
                token("2026-10-09T23:10:00Z", 120, 30.0),
                token("2026-10-09T23:10:01Z", 120, 31.0),
                token("2026-10-09T23:20:00Z", 240, 34.0),
            ],
        );
        let usage = &report.agents[0].1;
        assert_eq!(usage.last_5h.requests, 2);
        assert_eq!(usage.last_5h.input, 80);
        assert_eq!(usage.last_5h.cache_read, 120);
        assert_eq!(usage.last_5h.output, 40);
        assert_eq!(usage.models[0].0, "gpt-5.5");
        let limits = usage.limits.as_ref().unwrap();
        assert_eq!(limits.plan.as_deref(), Some("prolite"));
        assert_eq!(limits.windows[0].used_percent, 34.0);
        assert_eq!(limits.windows[0].label(), "5-hour");
        assert_eq!(limits.windows[1].label(), "Weekly");
    }

    #[test]
    fn pi_usage_reads_tokens_and_cost() {
        let line = format!(
            r#"{{"type":"message","id":"a","parentId":"b","timestamp":"{}","message":{{"role":"assistant","content":[{{"type":"toolCall","arguments":{{"input":"x"}}}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-opus-5-5","usage":{{"input":4,"output":191,"cacheRead":7,"cacheWrite":53151,"totalTokens":53353,"cost":{{"input":1.6e-05,"output":0.00382,"cacheRead":0,"cacheWrite":0.265755,"total":0.269591}}}},"timestamp":1790000000}}}}"#,
            iso(2)
        );
        let report = report_from_lines(AgentKind::Pi, &[line]);
        let usage = &report.agents[0].1;
        assert_eq!(usage.last_5h.input, 4);
        assert_eq!(usage.last_5h.output, 191);
        assert_eq!(usage.last_5h.cache_read, 7);
        assert_eq!(usage.last_5h.cache_write, 53151);
        assert!((usage.last_5h.cost_usd.unwrap() - 0.269591).abs() < 1e-9);
    }

    #[test]
    fn remote_output_matches_local_parsing() {
        // What `grep -oE` emits for one Claude line, preceded by its line number.
        let line = claude_line(1, "msg_r");
        let pattern = Regex::new(&REMOTE_PATTERNS.join("|")).unwrap();
        let mut output = "@@FILE /home/u/.claude/projects/p/s.jsonl\n7:\n".to_owned();
        for found in pattern.find_iter(&line) {
            output.push_str(found.as_str());
            output.push('\n');
        }
        let remote = parse_remote_output(&output, NOW);
        let local = report_from_lines(AgentKind::ClaudeCode, &[line]);
        assert_eq!(remote.agents, local.agents);
    }

    #[test]
    fn remote_output_keeps_pi_cost() {
        let line = format!(
            r#"{{"type":"message","timestamp":"{}","message":{{"role":"assistant","model":"m","usage":{{"input":4,"output":191,"cacheRead":7,"cacheWrite":5,"totalTokens":207,"cost":{{"input":0.1,"total":0.25}}}}}}}}"#,
            iso(1)
        );
        let pattern = Regex::new(&REMOTE_PATTERNS.join("|")).unwrap();
        let mut output = "@@FILE /home/u/.pi/agent/sessions/x/s.jsonl\n3:\n".to_owned();
        for found in pattern.find_iter(&line) {
            output.push_str(found.as_str());
            output.push('\n');
        }
        let remote = parse_remote_output(&output, NOW);
        assert_eq!(
            remote.agents,
            report_from_lines(AgentKind::Pi, &[line]).agents
        );
        assert_eq!(remote.agents[0].1.last_5h.cost_usd, Some(0.25));
    }

    #[test]
    fn remote_output_reads_claude_plan_limits() {
        let output = format!(
            "@@FILE /home/u/.claude/projects/p/s.jsonl\n7:\n{}\n@@CLAUDE_LIMITS {}\n{}\n",
            r#""usage":{"input_tokens":1,"output_tokens":2,"#,
            NOW - 60,
            r#"{"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1791600000},"seven_day":{"used_percentage":41.2,"resets_at":1792000000}}}"#
        );
        let report = parse_remote_output(&output, NOW);
        let limits = report.agents[0].1.limits.as_ref().unwrap();
        assert_eq!(report.agents[0].0, AgentKind::ClaudeCode);
        assert_eq!(limits.at, NOW - 60);
        assert_eq!(
            limits
                .windows
                .iter()
                .map(|w| (w.label(), w.used_percent))
                .collect::<Vec<_>>(),
            [("5-hour".to_owned(), 23.5), ("Weekly".to_owned(), 41.2)]
        );
    }

    #[test]
    fn remote_script_quotes_the_pattern_for_sh() {
        assert!(REMOTE_PATTERNS
            .iter()
            .all(|pattern| !pattern.contains('\'')));
        let script = remote_script();
        // `find -exec ... +` allows exactly one `{}`: its own placeholder.
        assert_eq!(script.matches("{}").count(), 1);
        assert!(script.contains("find \"$root\" -type f -name '*.jsonl' -mtime -8"));
        assert!(script.contains("last_token_usage"));
    }

    #[test]
    fn paths_identify_agents() {
        assert_eq!(
            AgentKind::from_path(r"C:\Users\u\.claude\projects\p\s.jsonl"),
            Some(AgentKind::ClaudeCode)
        );
        assert_eq!(
            AgentKind::from_path("/home/u/.codex/sessions/2026/10/10/r.jsonl"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            AgentKind::from_path("/home/u/.pi/agent/sessions/x/s.jsonl"),
            Some(AgentKind::Pi)
        );
        assert_eq!(AgentKind::from_path("/tmp/other.jsonl"), None);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(4_500_000), "4.5M");
        assert_eq!(format_until(NOW + 7800, NOW), "2h 10m");
        assert_eq!(format_until(NOW + 3 * 86_400 + 4 * 3600, NOW), "3d 4h");
    }
}
