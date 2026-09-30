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
    "#{?window_zoomed_flag,1,0}\t",
    "#{pane_id}\t",
    "#{window_id}",
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
    if fields.len() < 13 {
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
        pane_id: fields[11].to_string(),
        window_id: fields[12].to_string(),
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

/// A topology change, expanded into the exact tmux commands that will run.
///
/// Built as data rather than executed directly so the confirmation modal can
/// show every command before anything moves — a rearrange is the one thing in
/// tpx that changes the world it is describing, and "which panes am I about to
/// move" must be answerable before, not after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// One line for the modal title.
    pub title: String,
    /// Argument vectors, each run as `tmux <args…>`, in order.
    pub commands: Vec<Vec<String>>,
    /// What the reader should know before confirming — an empty window closing,
    /// tpx moving itself.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// The plan as it would be typed, for the confirmation modal.
    pub fn preview(&self) -> String {
        let mut text = self
            .commands
            .iter()
            .map(|args| format!("tmux {}", args.join(" ")))
            .collect::<Vec<_>>()
            .join("\n");
        for note in &self.notes {
            text.push_str("\n\n");
            text.push_str(note);
        }
        text
    }

    /// Run the commands in order, stopping at the first failure.
    ///
    /// A failure mid-plan leaves the earlier moves applied — they are separate
    /// tmux commands and there is no transaction to roll back — so the error
    /// names the step, and the caller refreshes to show the state that actually
    /// resulted rather than the one that was intended.
    pub fn run(&self) -> Result<()> {
        for (index, args) in self.commands.iter().enumerate() {
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            cmd::run("tmux", &argv, cmd::FAST).map_err(|error| {
                anyhow::anyhow!(
                    "step {} of {} (tmux {}): {error}",
                    index + 1,
                    self.commands.len(),
                    args.join(" ")
                )
            })?;
        }
        Ok(())
    }
}

/// `select-layout tiled` is appended once a plan moves more than one pane.
///
/// `join-pane` splits the destination pane, so the second pane joined lands in
/// half of the first one's half. Three panes joined by hand are unreadable
/// without a re-layout, while a single join is exactly the split the reader
/// asked for — hence the threshold rather than always tiling.
const TILE_THRESHOLD: usize = 2;

/// Move `sources` into the window holding `dest`, each as a split of `dest`.
///
/// This is both "merge these windows" and "move this pane over there": marking
/// a whole window marks its panes, and a window that loses its last pane is
/// closed by tmux, which is what merging means.
pub fn join_plan(sources: &[Pane], dest: &Pane, world: &[Pane]) -> Plan {
    let moving: Vec<&Pane> = sources
        .iter()
        // Joining a pane to itself is an error in tmux, and a pane already in
        // the destination window is already where the reader is asking to put
        // it — dropping both here keeps the plan honest about what will happen.
        .filter(|pane| pane.pane_id != dest.pane_id && pane.window_id != dest.window_id)
        .collect();

    let mut commands: Vec<Vec<String>> = moving
        .iter()
        .map(|pane| {
            vec![
                "join-pane".to_string(),
                // -d keeps the focus where it is: the reader is in tpx, and a
                // rearrange must not yank their client to the moved pane.
                "-d".to_string(),
                "-s".to_string(),
                pane.pane_id.clone(),
                "-t".to_string(),
                dest.pane_id.clone(),
            ]
        })
        .collect();
    if moving.len() >= TILE_THRESHOLD {
        commands.push(vec![
            "select-layout".to_string(),
            "-t".to_string(),
            dest.pane_id.clone(),
            "tiled".to_string(),
        ]);
    }

    Plan {
        title: format!(
            "move {} into {}:{}",
            describe(&moving),
            dest.session,
            dest.window_index
        ),
        commands,
        notes: emptied_windows(&moving, world),
    }
}

/// Break `sources` out into a window of their own.
///
/// The first pane is broken out to create the window; the rest join it. Pane
/// ids survive the break, so the join target is known up front and the plan
/// stays exact — no placeholder standing in for a window that does not exist
/// yet.
pub fn break_plan(sources: &[Pane], world: &[Pane]) -> Plan {
    let Some((first, rest)) = sources.split_first() else {
        return Plan {
            title: "nothing to move".to_string(),
            commands: Vec::new(),
            notes: Vec::new(),
        };
    };

    let mut commands = vec![vec![
        "break-pane".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        first.pane_id.clone(),
    ]];
    for pane in rest {
        commands.push(vec![
            "join-pane".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            pane.pane_id.clone(),
            "-t".to_string(),
            first.pane_id.clone(),
        ]);
    }
    if sources.len() >= TILE_THRESHOLD {
        commands.push(vec![
            "select-layout".to_string(),
            "-t".to_string(),
            first.pane_id.clone(),
            "tiled".to_string(),
        ]);
    }

    let refs: Vec<&Pane> = sources.iter().collect();
    Plan {
        title: format!("break {} out into a new window", describe(&refs)),
        commands,
        notes: emptied_windows(&refs, world),
    }
}

/// `2 panes (local:1.1, local:3.2)`, or the single target when there is one.
fn describe(panes: &[&Pane]) -> String {
    match panes {
        [] => "no panes".to_string(),
        [one] => format!("pane {}", one.target),
        many => format!(
            "{} panes ({})",
            many.len(),
            many.iter()
                .map(|pane| pane.target.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Windows that the move empties, which tmux then closes.
///
/// Stated up front because it is the surprising half of a merge: the panes
/// arrive where they were asked to, *and* a window the reader did not select
/// disappears. `world` is every pane on the server, so this compares what is
/// leaving against what the window actually holds — comparing against the
/// selection alone would call every move a closure.
fn emptied_windows(moving: &[&Pane], world: &[Pane]) -> Vec<String> {
    let mut notes = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for pane in moving {
        if seen.contains(&pane.window_id.as_str()) {
            continue;
        }
        seen.push(&pane.window_id);
        let in_window = world
            .iter()
            .filter(|other| other.window_id == pane.window_id)
            .count();
        let leaving = moving
            .iter()
            .filter(|other| other.window_id == pane.window_id)
            .count();
        if in_window > 0 && leaving == in_window {
            notes.push(format!(
                "window {}:{} ({}) is left with no panes and will close.",
                pane.session, pane.window_index, pane.window_name
            ));
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "local\t5\tccx\t1\t8691\t/Users/g/src/ccx\tfish\t1\t1\t1\t0\t%4\t@5\n\
                          local\t5\tccx\t2\t51179\t/Users/g/src/ccx\tcargo\t0\t1\t1\t0\t%9\t@5\n\
                          work\t2\tapi server\t1\t400\t/Users/g/api\tnode\t1\t0\t0\t1\t%11\t@7";

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

    fn pane(target: &str, pane_id: &str, window_id: &str) -> Pane {
        let (session, rest) = target.split_once(':').unwrap();
        let (window, index) = rest.split_once('.').unwrap();
        Pane {
            session: session.into(),
            window_index: window.parse().unwrap(),
            window_name: format!("w{window}"),
            pane_index: index.parse().unwrap(),
            target: target.into(),
            cwd: "/src".into(),
            current_command: "fish".into(),
            pid: 100,
            active: index == "1",
            window_active: false,
            session_attached: true,
            zoomed: false,
            pane_id: pane_id.into(),
            window_id: window_id.into(),
        }
    }

    fn world() -> Vec<Pane> {
        vec![
            pane("local:1.1", "%1", "@1"),
            pane("local:1.2", "%2", "@1"),
            pane("local:2.1", "%3", "@2"),
            pane("local:2.2", "%4", "@2"),
        ]
    }

    #[test]
    fn parses_the_stable_ids() {
        let panes = parse_panes(SAMPLE);
        assert_eq!(panes[1].pane_id, "%9");
        assert_eq!(panes[1].window_id, "@5");
    }

    // Pane indexes are positional: moving `local:2.1` renumbers `local:2.2` to
    // `local:2.1`, so a plan built from targets would move the wrong pane on its
    // second step. Every command must name `%id`.
    #[test]
    fn a_plan_addresses_panes_by_id_never_by_index() {
        let world = world();
        let plan = join_plan(&world[2..], &world[0], &world);
        for args in &plan.commands {
            assert!(
                args.iter().all(|arg| !arg.contains(':')),
                "index-shaped target in {args:?}"
            );
        }
    }

    #[test]
    fn join_moves_each_source_into_the_destination_pane() {
        let world = world();
        let plan = join_plan(&world[2..4], &world[0], &world);
        assert_eq!(
            plan.commands[0],
            ["join-pane", "-d", "-s", "%3", "-t", "%1"]
        );
        assert_eq!(
            plan.commands[1],
            ["join-pane", "-d", "-s", "%4", "-t", "%1"]
        );
    }

    #[test]
    fn joining_more_than_one_pane_retiles_the_destination() {
        let world = world();
        let single = join_plan(&world[2..3], &world[0], &world);
        assert!(
            !single
                .commands
                .iter()
                .any(|args| args[0] == "select-layout"),
            "a single join is the split the reader asked for"
        );
        let both = join_plan(&world[2..4], &world[0], &world);
        assert_eq!(
            both.commands.last().unwrap(),
            &["select-layout", "-t", "%1", "tiled"]
        );
    }

    #[test]
    fn join_skips_panes_already_in_the_destination_window() {
        let world = world();
        // Both sources live in window 1, which is where the destination is.
        let plan = join_plan(&world[0..2], &world[0], &world);
        assert!(plan.is_empty());
    }

    #[test]
    fn merging_a_whole_window_says_that_the_window_will_close() {
        let world = world();
        let plan = join_plan(&world[2..4], &world[0], &world);
        assert!(
            plan.notes.iter().any(|note| note.contains("local:2")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn moving_part_of_a_window_does_not_claim_it_closes() {
        let world = world();
        let plan = join_plan(&world[2..3], &world[0], &world);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }

    #[test]
    fn break_puts_the_first_pane_in_a_new_window_and_gathers_the_rest_there() {
        let world = world();
        let plan = break_plan(&world[0..2], &world);
        assert_eq!(plan.commands[0], ["break-pane", "-d", "-s", "%1"]);
        // %1 keeps its id through the break, so the join target is exact rather
        // than a placeholder for a window that does not exist yet.
        assert_eq!(
            plan.commands[1],
            ["join-pane", "-d", "-s", "%2", "-t", "%1"]
        );
        assert_eq!(
            plan.commands.last().unwrap(),
            &["select-layout", "-t", "%1", "tiled"]
        );
    }

    #[test]
    fn a_preview_shows_every_command_and_the_notes() {
        let world = world();
        let plan = join_plan(&world[2..4], &world[0], &world);
        let preview = plan.preview();
        assert_eq!(preview.matches("tmux ").count(), plan.commands.len());
        assert!(preview.contains("will close"));
    }

    #[test]
    fn skips_malformed_lines() {
        assert!(parse_panes("garbage\tline").is_empty());
        assert!(parse_panes("").is_empty());
    }
}
