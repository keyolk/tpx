//! Host process table via `ps`, plus lazily-collected per-process detail.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;

use super::cmd;
use crate::model::{Proc, ProcKey};

/// Every process on the host, plus the pid->children index.
///
/// `command` must stay last in the format string — it is the only field that
/// can contain spaces. macOS `ps` has no `nlwp`/`thcount` keyword, so thread
/// counts are filled in later per-process by [`thread_count`].
///
/// `cputime` is collected so the caller can derive *current* cpu from the delta
/// between two snapshots. `ps`'s own `%cpu` on macOS is a lifetime average
/// (total cpu time ÷ elapsed time), which for a days-old process reads ~0% even
/// while it is pinning a core — the opposite of what this tool is for.
pub fn processes() -> Result<ProcessTable> {
    let raw = cmd::run(
        "ps",
        &["-axo", "pid=,ppid=,rss=,stat=,etime=,time=,command="],
        cmd::FAST,
    )?;
    Ok(parse_ps(&raw))
}

/// The host process table: every process keyed by [`ProcKey`], plus the
/// pid -> children index the tree walk needs.
pub type ProcessTable = (HashMap<ProcKey, Proc>, HashMap<u32, Vec<u32>>);

fn parse_ps(raw: &str) -> ProcessTable {
    let mut procs = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in raw.lines() {
        let Some(proc) = parse_ps_line(line) else {
            continue;
        };
        children.entry(proc.ppid).or_default().push(proc.key.pid);
        procs.insert(proc.key.clone(), proc);
    }
    // Stable child order keeps the tree from reshuffling between snapshots —
    // ps output order is not guaranteed, and a jumping tree is unreadable.
    for kids in children.values_mut() {
        kids.sort_unstable();
    }
    (procs, children)
}

fn parse_ps_line(line: &str) -> Option<Proc> {
    // `ps` pads its numeric columns, so runs of spaces separate fields —
    // `splitn` on a whitespace predicate would yield empty fields and eat the
    // split budget before reaching the command.
    let (fields, command) = split_leading_fields(line, 6)?;
    let pid: u32 = fields[0].parse().ok()?;
    let ppid: u32 = fields[1].parse().ok()?;
    // ps reports rss in KiB.
    let rss_kib: u64 = fields[2].parse().unwrap_or(0);
    let state = fields[3].to_string();
    let age_secs = parse_etime(fields[4])?;
    let cpu_time_secs = parse_cputime(fields[5]).unwrap_or(0.0);

    Some(Proc {
        key: ProcKey::host(pid),
        ppid,
        command: command.to_string(),
        age_secs,
        // Filled by the caller from the delta against the previous snapshot;
        // a single sample cannot know current cpu.
        cpu_pct: 0.0,
        cpu_time_secs,
        rss_bytes: rss_kib * 1024,
        state,
        threads: None,
        fd_count: None,
    })
}

/// `ps` cputime, as `mm:ss.ff` or `hh:mm:ss.ff`.
fn parse_cputime(field: &str) -> Option<f64> {
    let mut parts = field.trim().split(':').rev();
    let seconds: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next().map_or(Ok(0.0), str::parse).ok()?;
    let hours: f64 = parts.next().map_or(Ok(0.0), str::parse).ok()?;
    Some((hours * 60.0 + minutes) * 60.0 + seconds)
}

/// Take `count` whitespace-separated fields off the front, returning them plus
/// the untouched remainder — which is the command line, spaces and all.
pub fn split_leading_fields(line: &str, count: usize) -> Option<(Vec<&str>, &str)> {
    let mut fields = Vec::with_capacity(count);
    let mut rest = line.trim_start();
    for _ in 0..count {
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    Some((fields, rest.trim_end()))
}

/// `ps` etime, in any of `ss`, `mm:ss`, `hh:mm:ss`, `dd-hh:mm:ss`.
fn parse_etime(field: &str) -> Option<u64> {
    let (days, rest) = match field.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().ok()?, rest),
        None => (0, field),
    };
    let mut parts = rest.split(':').rev();
    let seconds: u64 = parts.next()?.trim().parse().ok()?;
    let minutes: u64 = parts.next().map_or(Ok(0), str::parse).ok()?;
    let hours: u64 = parts.next().map_or(Ok(0), str::parse).ok()?;
    Some(((days * 24 + hours) * 60 + minutes) * 60 + seconds)
}

/// Thread count for one host process. Separate from the bulk `ps` because
/// macOS exposes threads only as one output row per thread (`ps -M`), which is
/// far too expensive to run for every process on every refresh.
pub fn thread_count(pid: u32) -> Result<u32> {
    let raw = cmd::run(
        "ps",
        &["-M", "-p", &pid.to_string()],
        Duration::from_secs(2),
    )?;
    // One header row, then one row per thread.
    Ok(raw
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .count() as u32)
}

/// Open file-descriptor count for one host process. `lsof -p` on a single pid
/// is ~40ms; a full-table `lsof` is not, so this stays lazy.
pub fn fd_count(pid: u32) -> Result<u32> {
    let raw = cmd::run(
        "lsof",
        &["-nP", "-p", &pid.to_string()],
        Duration::from_secs(4),
    )?;
    Ok(raw
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .count() as u32)
}

/// Non-network open files for one host process, as `(fd, type, path)` rows —
/// the file/IO drill-down. Sockets are excluded; they have their own panel.
pub fn open_files(pid: u32) -> Result<Vec<OpenFile>> {
    let raw = cmd::run(
        "lsof",
        &["-nP", "-p", &pid.to_string(), "-F", "ftn"],
        Duration::from_secs(4),
    )?;
    Ok(parse_open_files(&raw))
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenFile {
    pub fd: String,
    pub kind: String,
    pub path: String,
}

/// `lsof -F ftn` emits one field per line, tagged by its first character,
/// grouped per file: `f<fd>`, `t<type>`, `n<name>`.
fn parse_open_files(raw: &str) -> Vec<OpenFile> {
    let mut files = Vec::new();
    let mut fd = String::new();
    let mut kind = String::new();
    for line in raw.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "f" => {
                fd = value.to_string();
                kind.clear();
            }
            "t" => kind = value.to_string(),
            "n" => {
                // Sockets belong to the network panel, and the pipe/kqueue
                // noise that dominates an fd table hides the real files.
                let is_socket = matches!(kind.as_str(), "IPv4" | "IPv6" | "unix" | "systm");
                if !is_socket && !value.is_empty() {
                    files.push(OpenFile {
                        fd: fd.clone(),
                        kind: kind.clone(),
                        path: value.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real `ps -axo pid=,ppid=,rss=,stat=,etime=,time=,command=` output.
    const SAMPLE: &str = "    1     0  17008 Ss   12-10:30:53   41:56.13 /sbin/launchd\n\
                          \x20 140     1  18144 S    08-13:34:46    0:12.04 /Applications/Grammarly Desktop.app/Contents/MacOS/Helper --flag\n\
                          \x209923  9542  13504 Ss+  22:04:14       1:09.25 fish (kiro-cli-term)\n\
                          \x20 532     1  22320 Ss   00:31          0:00.31 /usr/libexec/logd";

    #[test]
    fn parses_pid_ppid_and_command_with_spaces() {
        let (procs, _) = parse_ps(SAMPLE);
        let grammarly = &procs[&ProcKey::host(140)];
        assert_eq!(grammarly.ppid, 1);
        assert!(grammarly.command.ends_with("Helper --flag"));
        assert!(grammarly.command.contains("Grammarly Desktop.app"));
    }

    #[test]
    fn converts_rss_kib_to_bytes() {
        let (procs, _) = parse_ps(SAMPLE);
        assert_eq!(procs[&ProcKey::host(9923)].rss_bytes, 13_504 * 1024);
    }

    #[test]
    fn collects_cpu_time_rather_than_ps_lifetime_average() {
        let (procs, _) = parse_ps(SAMPLE);
        // 41:56.13 of cpu time. `ps` would report this as 0.2% %cpu because the
        // process is 12 days old; the raw counter is what a rate can be built on.
        assert_eq!(procs[&ProcKey::host(1)].cpu_time_secs, 41.0 * 60.0 + 56.13);
        // No rate is claimed from a single sample.
        assert_eq!(procs[&ProcKey::host(1)].cpu_pct, 0.0);
    }

    #[test]
    fn parses_command_names_that_look_like_parentheses() {
        let (procs, _) = parse_ps(SAMPLE);
        assert_eq!(procs[&ProcKey::host(9923)].name(), "fish");
    }

    #[test]
    fn builds_sorted_children_index() {
        let (_, children) = parse_ps(SAMPLE);
        assert_eq!(children[&1], vec![140, 532]);
        assert_eq!(children[&9542], vec![9923]);
    }

    #[test]
    fn parses_cputime_in_both_formats() {
        assert_eq!(parse_cputime("0:00.31"), Some(0.31));
        assert_eq!(parse_cputime("1:09.25"), Some(69.25));
        assert_eq!(parse_cputime("41:56.13"), Some(41.0 * 60.0 + 56.13));
        assert_eq!(
            parse_cputime("2:03:04.50"),
            Some(2.0 * 3600.0 + 3.0 * 60.0 + 4.5)
        );
    }

    #[test]
    fn parses_every_etime_format() {
        assert_eq!(parse_etime("31"), Some(31));
        assert_eq!(parse_etime("00:31"), Some(31));
        assert_eq!(parse_etime("22:04:14"), Some(22 * 3600 + 4 * 60 + 14));
        assert_eq!(
            parse_etime("12-10:30:53"),
            Some(12 * 86_400 + 10 * 3600 + 30 * 60 + 53)
        );
        assert_eq!(parse_etime("garbage"), None);
    }

    #[test]
    fn open_files_excludes_sockets_and_keeps_regular_files() {
        let raw = "f3\ntREG\nn/Users/g/src/app.log\n\
                   f5\ntIPv4\nn127.0.0.1:8080\n\
                   f7\ntDIR\nn/Users/g/src\n";
        let files = parse_open_files(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0],
            OpenFile {
                fd: "3".into(),
                kind: "REG".into(),
                path: "/Users/g/src/app.log".into(),
            }
        );
        assert_eq!(files[1].kind, "DIR");
    }
}
