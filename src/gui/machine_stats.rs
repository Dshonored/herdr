//! CPU, memory, and network use of the selected machine.
//!
//! One POSIX script samples for a second (Linux `/proc`, macOS `top`/`vm_stat`/`netstat`)
//! and prints `cpu <percent>`, `mem <used> <total>` (bytes) and `net <rx> <tx>` (bytes per
//! second). Local runs it with `sh`; saved SSH machines run it over a shared ssh master so
//! frequent refreshes don't pay for a new login each time.

use std::io::{self, Write as _};
use std::process::{Command, Stdio};

const SCRIPT: &str = r#"
if [ -r /proc/stat ]; then
  cpu() { awk '/^cpu /{print $2+$3+$4+$7+$8+$9, $2+$3+$4+$5+$6+$7+$8+$9}' /proc/stat; }
  net() { awk 'NR>2{sub(/^ */,""); split($0,a,":"); if(a[1]!="lo"){split(a[2],f," "); rx+=f[1]; tx+=f[9]}} END{print rx+0, tx+0}' /proc/net/dev; }
  c1=$(cpu); n1=$(net); sleep 1; c2=$(cpu); n2=$(net)
  echo "$c1 $c2" | awk '{t=$4-$2; print "cpu", (t>0 ? 100*($3-$1)/t : 0)}'
  awk '/^MemTotal:/{t=$2}/^MemAvailable:/{a=$2} END{print "mem", (t-a)*1024, t*1024}' /proc/meminfo
else
  net() { netstat -ib | awk '$3 ~ /^<Link/ && $1 !~ /^lo/{rx+=$(NF-4); tx+=$(NF-1)} END{print rx+0, tx+0}'; }
  top -l 2 -n 0 -s 1 | awk '/^CPU usage/{u=$3; s=$5} END{print "cpu", u+s}' &
  n1=$(net); sleep 1; n2=$(net); wait
  vm_stat | awk -v p="$(sysctl -n hw.pagesize)" -v t="$(sysctl -n hw.memsize)" '/^Pages active/{a=$3}/^Pages wired/{w=$4}/occupied by compressor/{c=$5} END{print "mem", (a+w+c)*p, t}'
fi
echo "$n1 $n2" | awk '{print "net", $3-$1, $4-$2}'
"#;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MachineStats {
    pub(crate) cpu_percent: f64,
    pub(crate) mem_used: u64,
    pub(crate) mem_total: u64,
    pub(crate) net_rx_per_sec: u64,
    pub(crate) net_tx_per_sec: u64,
}

impl MachineStats {
    pub(crate) fn mem_percent(&self) -> f64 {
        if self.mem_total == 0 {
            0.0
        } else {
            100.0 * self.mem_used as f64 / self.mem_total as f64
        }
    }
}

fn parse(output: &str) -> Option<MachineStats> {
    let mut stats = MachineStats::default();
    let (mut cpu, mut mem) = (false, false);
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let key = fields.next();
        let mut num = || fields.next().and_then(|value| value.parse::<f64>().ok());
        match key {
            Some("cpu") => {
                stats.cpu_percent = num()?.clamp(0.0, 100.0);
                cpu = true;
            }
            Some("mem") => {
                stats.mem_used = num()? as u64;
                stats.mem_total = num()? as u64;
                mem = true;
            }
            Some("net") => {
                // Counters can reset (interface down/up) and go backwards.
                stats.net_rx_per_sec = num()?.max(0.0) as u64;
                stats.net_tx_per_sec = num()?.max(0.0) as u64;
            }
            _ => {}
        }
    }
    (cpu && mem).then_some(stats)
}

/// Samples this machine, or `target` over SSH.
pub(crate) fn sample(target: Option<&str>) -> io::Result<MachineStats> {
    let mut command = match target {
        None => Command::new("sh"),
        Some(target) => {
            let mut command = Command::new("ssh");
            command.args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ControlMaster=auto",
                "-o",
                // ponytail: /tmp keeps the socket path under the 104-byte limit macOS's
                // long $TMPDIR would break.
                "ControlPath=/tmp/herdr-stats-%C",
                "-o",
                "ControlPersist=60",
                target,
                "sh -s",
            ]);
            command
        }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(SCRIPT.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    parse(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| io::Error::other("couldn't read this machine's CPU and memory"))
}

/// Compact byte size, e.g. `512 B`, `12.3 KB`, `4.5 GB`.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_script_output_in_any_order() {
        let stats = parse("mem 8589934592 17179869184\nnet 2048 -5\ncpu 12.5\n").unwrap();
        assert_eq!(stats.cpu_percent, 12.5);
        assert_eq!(stats.mem_percent(), 50.0);
        assert_eq!((stats.net_rx_per_sec, stats.net_tx_per_sec), (2048, 0));
    }

    #[test]
    fn missing_cpu_or_memory_is_an_error() {
        assert_eq!(parse("net 1 2\n"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GB");
    }
}
