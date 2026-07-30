//! tmux topology: every pane on the server, with the window/session context
//! needed to group them.

use std::time::Duration;

use anyhow::Result;

use super::cmd;
use crate::model::Pane;

/// Field separator for `tmux -F`. Tab is safe: pane paths and window names can
/// contain spaces, but not tabs.
const SEP: char = '\t';

const FORMAT: &str = concat!(
    "#{session_name}\t",
    "#{window_index}\t",
    "#{window_name}\t",
    "#{pane_index}\t",
    "#{pane_pid}\t",
    "#{pane_current_path}\t",
    "#{pane_current_command}\t",
    "#{?pane_active,1,0}\t",
    "#{?window_active,1,0}\t",
    "#{?session_attached,1,0}\t",
    "#{?window_zoomed_flag,1,0}",
);

/// All panes across all sessions on the tmux server.
pub fn panes() -> Result<Vec<Pane>> {
    let raw = cmd::run("tmux", &["list-panes", "-a", "-F", FORMAT], cmd::FAST)?;
    Ok(parse_panes(&raw))
}

fn parse_panes(raw: &str) -> Vec<Pane> {
    raw.lines().filter_map(parse_pane).collect()
}

fn parse_pane(line: &str) -> Option<Pane> {
    let fields: Vec<&str> = line.split(SEP).collect();
    if fields.len() < 11 {
        return None;
    }
    let session = fields[0].to_string();
    let window_index: u32 = fields[1].parse().ok()?;
    let pane_index: u32 = fields[3].parse().ok()?;
    Some(Pane {
        target: format!("{session}:{window_index}.{pane_index}"),
        session,
        window_index,
        window_name: fields[2].to_string(),
        pane_index,
        pid: fields[4].parse().ok()?,
        cwd: fields[5].to_string(),
        current_command: fields[6].to_string(),
        active: fields[7] == "1",
        window_active: fields[8] == "1",
        session_attached: fields[9] == "1",
        zoomed: fields[10] == "1",
    })
}

/// Last `lines` rows of a pane's visible output plus scrollback, for the
/// "what was this pane doing" view. `-J` joins wrapped lines so a long build
/// command reads as one line.
pub fn capture_pane(target: &str, lines: u16) -> Result<String> {
    // -S -N starts N lines back from the top of the visible area; -E - ends at
    // the bottom of the visible area (not the end of history), which is what
    // the user actually sees.
    let start = format!("-{lines}");
    cmd::run(
        "tmux",
        &[
            "capture-pane",
            "-p",
            "-J",
            "-t",
            target,
            "-S",
            &start,
            "-E",
            "-",
        ],
        Duration::from_secs(2),
    )
}

/// Switch the client's focus to a pane. Used by `Enter` on a pane row.
pub fn switch_to(target: &str) -> Result<()> {
    // switch-client handles the cross-session case that select-window alone
    // cannot; both are needed because select-pane is window-local.
    let (session, rest) = target.split_once(':').unwrap_or((target, ""));
    cmd::run("tmux", &["switch-client", "-t", session], cmd::FAST)?;
    if !rest.is_empty() {
        cmd::run("tmux", &["select-window", "-t", target], cmd::FAST)?;
        cmd::run("tmux", &["select-pane", "-t", target], cmd::FAST)?;
    }
    Ok(())
}

/// Whether we are running inside tmux. Determines whether `switch-client`
/// can do anything useful.
pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// The window tpx itself is running in, as `(session, window_index)`.
///
/// Resolved from `$TMUX_PANE` rather than from the client's *current* window:
/// the two differ the moment the reader switches windows while tpx keeps
/// running, and the useful answer is "the window tpx lives in", which stays
/// stable.
///
/// `None` when not inside tmux, or when the pane id no longer resolves —
/// `display-message` exits 0 with empty output for a stale id, so the empty
/// case must be checked explicitly.
pub fn current_window() -> Option<(String, u32)> {
    let pane = std::env::var("TMUX_PANE").ok()?;
    let raw = cmd::run(
        "tmux",
        &[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{session_name}\t#{window_index}",
        ],
        cmd::FAST,
    )
    .ok()?;
    let line = raw.lines().next()?.trim();
    let (session, index) = line.split_once(SEP)?;
    if session.is_empty() {
        return None;
    }
    Some((session.to_string(), index.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "local\t5\tccx\t1\t8691\t/Users/g/src/ccx\tfish\t1\t1\t1\t0\n\
                          local\t5\tccx\t2\t51179\t/Users/g/src/ccx\tcargo\t0\t1\t1\t0\n\
                          work\t2\tapi server\t1\t400\t/Users/g/api\tnode\t1\t0\t0\t1";

    #[test]
    fn parses_all_panes_with_targets() {
        let panes = parse_panes(SAMPLE);
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].target, "local:5.1");
        assert_eq!(panes[2].target, "work:2.1");
    }

    #[test]
    fn parses_window_names_containing_spaces() {
        let panes = parse_panes(SAMPLE);
        assert_eq!(panes[2].window_name, "api server");
    }

    #[test]
    fn parses_flags() {
        let panes = parse_panes(SAMPLE);
        assert!(panes[0].active && panes[0].window_active && panes[0].session_attached);
        assert!(!panes[1].active);
        assert!(!panes[2].session_attached);
        assert!(panes[2].zoomed);
    }

    #[test]
    fn skips_malformed_lines() {
        assert!(parse_panes("garbage\tline").is_empty());
        assert!(parse_panes("").is_empty());
    }
}
