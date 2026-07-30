//! What a process is doing *right now*, and what environment it runs in.
//!
//! Two questions the process table cannot answer:
//!
//! - **Why is this busy (or stuck)?** `sample` walks the live task's stacks. It
//!   needs no privileges for a process you own, which makes it the one deep
//!   introspection macOS allows here — `dtrace` does not survive SIP.
//! - **Why is it behaving that way?** `ps eww` exposes the environment it was
//!   started with: the flags, endpoints and feature toggles that explain it.
//!
//! Both are on-demand: `sample` costs a wall-clock second by construction, and an
//! environment block is hundreds of variables.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, bail};

use super::cmd;

/// A summarized stack sample: what each thread was doing, deepest frame first.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// One entry per thread that appeared in the sample.
    pub threads: Vec<ThreadSample>,
    /// Milliseconds actually sampled.
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSample {
    /// Thread label as `sample` prints it, e.g. `com.apple.main-thread`.
    pub label: String,
    /// Samples attributed to this thread — its share of the process's time.
    pub samples: u32,
    /// The innermost frame: where the thread actually sat. This is the answer —
    /// `read (in libsystem_kernel.dylib)` means "waiting on input", not "busy".
    pub leaf: String,
    /// A few frames above the leaf, outermost-last, for context.
    pub stack: Vec<String>,
}

impl ThreadSample {
    /// Whether the leaf is a kernel wait rather than real work.
    ///
    /// The distinction matters: a thread parked in `read`/`select`/`kevent` is
    /// idle, and reporting it the same way as a hot loop would invert the answer.
    pub fn is_waiting(&self) -> bool {
        // Prefix matching, not equality: the real symbols carry suffixes
        // (`kevent64`, `read_nocancel`, `__psynch_cvwait`), and requiring an exact
        // match reported an idle thread as busy — the opposite of the truth.
        const WAITS: [&str; 10] = [
            "read",
            "select",
            "kevent",
            "poll",
            "wait4",
            "semaphore_wait",
            "psynch_cvwait",
            "ulock_wait",
            "mach_msg",
            "accept",
        ];
        let leaf = self.leaf.split_whitespace().next().unwrap_or("");
        // Leading underscores are an implementation detail of the libc symbol.
        let leaf = leaf.trim_start_matches('_');
        WAITS.iter().any(|wait| leaf.starts_with(wait))
    }
}

/// Frames of context kept above each leaf. Enough to see the calling path,
/// short enough that several threads still fit a pane.
const STACK_CONTEXT: usize = 6;

/// Sample a process's stacks for `millis`.
///
/// `sample` writes its report to a temp file and prints the path, so the file is
/// read and then removed — leaving reports behind in `/tmp` would be litter the
/// caller never asked for.
pub fn sample(pid: u32, millis: u64) -> Result<Sample> {
    let seconds = millis.div_ceil(1000).max(1);
    // -f writes to a path we choose, so there is no output to parse for one and no
    // guessing which file in /tmp is ours.
    let path = std::env::temp_dir().join(format!("tpx-sample-{pid}.txt"));
    let path_arg = path.to_string_lossy().to_string();

    let result = cmd::run(
        "sample",
        &[&pid.to_string(), &seconds.to_string(), "-f", &path_arg],
        // The sample itself takes `seconds`; symbol processing adds to that.
        Duration::from_secs(seconds + 20),
    );
    if let Err(error) = result {
        let _ = std::fs::remove_file(&path);
        bail!("sample failed: {error}");
    }

    let report = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    let report = report?;

    let threads = parse_call_graph(&report);
    if threads.is_empty() {
        bail!("sample produced no call graph — the process may have exited");
    }
    Ok(Sample {
        threads,
        duration_ms: seconds * 1000,
    })
}

/// Parse the `Call graph:` section of a `sample` report.
///
/// The format is an indented tree of `<samples> <frame>` lines. Two details make a
/// naive reader wrong:
///
/// - Lines carry **branch drawing characters** (`+`, `!`, `|`, `:`) before the
///   sample count, so the count is not simply the first token.
/// - A stack **forks** when threads diverge mid-call: one child may hold 860
///   samples and its sibling 2. Taking the last line as the leaf picks whichever
///   branch printed last, which is usually the 2-sample one — reporting a rare
///   path as if it were what the thread is doing.
///
/// So each thread is walked as a tree and the *heaviest* path is followed down.
fn parse_call_graph(report: &str) -> Vec<ThreadSample> {
    let mut threads: Vec<ThreadSample> = Vec::new();
    // Frames of the thread being read, as (indent depth, samples, frame).
    let mut frames: Vec<(usize, u32, String)> = Vec::new();

    let mut in_graph = false;
    for line in report.lines() {
        if line.starts_with("Call graph:") {
            in_graph = true;
            continue;
        }
        if !in_graph {
            continue;
        }
        // The graph ends at the first unindented section (`Binary Images:` etc.).
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }

        let Some((depth, samples, frame)) = split_frame(line) else {
            continue;
        };
        if let Some(label) = thread_label(&frame) {
            finish_thread(&mut threads, &mut frames);
            threads.push(ThreadSample {
                label,
                samples,
                leaf: String::new(),
                stack: Vec::new(),
            });
            continue;
        }
        if !threads.is_empty() {
            frames.push((depth, samples, clean_frame(&frame)));
        }
    }
    finish_thread(&mut threads, &mut frames);
    // Busiest thread first — it is the one being asked about.
    threads.sort_by(|a, b| b.samples.cmp(&a.samples));
    threads
}

/// Attach the heaviest path through the collected frames to their thread.
fn finish_thread(threads: &mut [ThreadSample], frames: &mut Vec<(usize, u32, String)>) {
    let Some(thread) = threads.last_mut() else {
        frames.clear();
        return;
    };
    let path = heaviest_path(frames);
    if let Some(leaf) = path.last() {
        thread.leaf = leaf.clone();
        let start = path.len().saturating_sub(STACK_CONTEXT + 1);
        // Innermost-first: the leaf is the answer, the rest is context.
        thread.stack = path[start..].iter().rev().cloned().collect();
    }
    frames.clear();
}

/// Follow the child with the most samples at each level.
///
/// Frames arrive in depth-first order with an indent depth, so a frame's children
/// are the following frames at `depth + 1` until the depth drops back.
fn heaviest_path(frames: &[(usize, u32, String)]) -> Vec<String> {
    let mut path = Vec::new();
    let mut index = 0usize;
    let mut expected_depth = frames.first().map(|(depth, _, _)| *depth);

    while let Some(depth) = expected_depth {
        // Candidates: the frames at this depth within the current subtree.
        let mut best: Option<(usize, u32)> = None;
        let mut cursor = index;
        while cursor < frames.len() {
            let (frame_depth, samples, _) = &frames[cursor];
            if *frame_depth < depth {
                break; // Left the subtree.
            }
            if *frame_depth == depth && best.is_none_or(|(_, top)| *samples > top) {
                best = Some((cursor, *samples));
            }
            cursor += 1;
        }
        let Some((chosen, _)) = best else { break };
        path.push(frames[chosen].2.clone());

        // Descend into the chosen frame's children.
        index = chosen + 1;
        expected_depth = frames
            .get(index)
            .filter(|(child_depth, _, _)| *child_depth > depth)
            .map(|(child_depth, _, _)| *child_depth);
    }
    path
}

/// `    +   862 some_frame  (in lib) + 12  [0x...]` -> `(depth, 862, "some_frame …")`
///
/// Depth comes from the column the sample count starts at, since the branch
/// characters and indentation together encode nesting.
fn split_frame(line: &str) -> Option<(usize, u32, String)> {
    // Strip leading spaces and the tree-drawing characters that precede the count.
    let mut offset = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' | '+' | '!' | '|' | ':' => offset += ch.len_utf8(),
            _ => break,
        }
    }
    let rest = &line[offset..];
    let (count, frame) = rest.split_once(' ')?;
    let samples: u32 = count.parse().ok()?;
    Some((offset, samples, frame.trim_start().to_string()))
}

/// The human label of a thread header line, if this is one.
fn thread_label(frame: &str) -> Option<String> {
    let rest = frame.strip_prefix("Thread_")?;
    // `4453802   DispatchQueue_1: com.apple.main-thread  (serial)`
    let after_id = rest.split_once(' ').map(|(_, rest)| rest).unwrap_or("");
    let label = after_id.trim();
    if label.is_empty() {
        // A thread with no queue name still needs an identity.
        return Some(format!("thread {}", rest.trim()));
    }
    Some(label.to_string())
}

/// Drop the address and offset noise, keeping the symbol and its library.
///
/// An unsymbolized frame prints as `???  (in claude.exe)  load address 0x… + 0x…`,
/// which is all noise — it becomes just the binary, so a stack of them still says
/// *which* binary is running even when the symbols are stripped.
fn clean_frame(frame: &str) -> String {
    if let Some(rest) = frame.strip_prefix("???  (in ") {
        let binary = rest.split(')').next().unwrap_or("?");
        return format!("[{binary}]");
    }
    let frame = match frame.split_once("  [0x") {
        Some((head, _)) => head,
        None => frame,
    };
    // `+ 2236` offsets say nothing without a disassembler. A deduplicated symbol
    // carries several at once (`+ 0,136`), so commas count as offset text too.
    let frame = match frame.rsplit_once(" + ") {
        Some((head, tail))
            if !tail.is_empty()
                && tail.chars().all(|c| {
                    c.is_ascii_digit() || c == ',' || c == 'x' || c.is_ascii_hexdigit()
                }) =>
        {
            head
        }
        _ => frame,
    };
    frame.trim().to_string()
}

/// The environment a process was started with.
///
/// `ps eww` only reveals this for processes the caller owns, which is exactly the
/// set tpx cares about. Values are returned verbatim; masking is the caller's
/// decision because it depends on how the data is displayed.
pub fn environment(pid: u32) -> Result<Vec<(String, String)>> {
    let raw = cmd::run(
        "ps",
        &["eww", "-o", "command=", "-p", &pid.to_string()],
        cmd::FAST,
    )?;
    let vars = parse_environment(&raw);
    if vars.is_empty() {
        bail!("no environment visible (process may belong to another user)");
    }
    Ok(vars)
}

/// `ps eww` prints the command line, then the environment, space-separated —
/// with no delimiter between the two.
///
/// The split is heuristic and cannot be otherwise: a `KEY=value` token is assumed
/// to start the environment, and the first such token wins. A command line whose
/// own arguments contain `=` (`--settings={"a":1}`) would cut early, so tokens
/// that do not look like shell-legal names are skipped.
fn parse_environment(raw: &str) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;

    for token in raw.split_whitespace() {
        match split_env_token(token) {
            Some((key, value)) => {
                if let Some(var) = current.take() {
                    vars.push(var);
                }
                current = Some((key, value));
            }
            // Not a new assignment: either still inside a value with spaces, or
            // part of the command line before the environment began.
            None => {
                if let Some((_, value)) = current.as_mut() {
                    value.push(' ');
                    value.push_str(token);
                }
            }
        }
    }
    if let Some(var) = current {
        vars.push(var);
    }
    vars
}

/// A `KEY=value` token whose key is a shell-legal environment name.
///
/// The rule has to serve two opposing cases seen on real processes:
///
/// - Lowercase names are common (`color_black=colour232`, exported from a tmux
///   config), so an uppercase-only rule swallowed them into the *previous*
///   variable's value — producing `COMMAND_MODE=unix2003 not_tmux=ps -o state=…`.
/// - Command-line flags contain `=` (`--settings={"a":1}`), and treating those as
///   variables would cut the command line off mid-argument.
///
/// A shell-legal identifier — letters, digits, `_`, never leading with a digit and
/// never containing `-`, `/`, `.` or quotes — separates the two.
fn split_env_token(token: &str) -> Option<(String, String)> {
    let (key, value) = token.split_once('=')?;
    let mut chars = key.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let legal = chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    legal.then(|| (key.to_string(), value.to_string()))
}

/// Environment variable names whose values are hidden by default.
///
/// A process listing is a place secrets leak by accident — a token pasted into a
/// screen share or an issue. The name is still shown, because knowing that a
/// variable is *set* is often the whole question.
pub fn is_sensitive(key: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "KEY",
        "CREDENTIAL",
        "AUTH",
        "SESSION",
    ];
    MARKERS.iter().any(|marker| key.contains(marker))
}

/// Value as it should be displayed: masked when the name looks sensitive.
pub fn display_value(key: &str, value: &str) -> String {
    if !is_sensitive(key) {
        return value.to_string();
    }
    if value.is_empty() {
        return String::new();
    }
    // The length is a useful signal (empty vs set vs plausible-looking) and does
    // not leak the value.
    format!("<hidden, {} chars>", value.chars().count())
}

/// Group environment variables so a hundred entries are navigable.
///
/// Ordering is by how likely a variable explains behavior, not alphabetical: the
/// process's own configuration first, the ambient shell environment last.
pub fn grouped(vars: &[(String, String)]) -> Vec<(&'static str, Vec<(String, String)>)> {
    let mut sensitive = Vec::new();
    let mut path_like = Vec::new();
    let mut other = Vec::new();

    for (key, value) in vars {
        if is_sensitive(key) {
            sensitive.push((key.clone(), value.clone()));
        } else if key == "PATH" || key.ends_with("_PATH") || key.ends_with("_HOME") {
            path_like.push((key.clone(), value.clone()));
        } else {
            other.push((key.clone(), value.clone()));
        }
    }
    for group in [&mut sensitive, &mut path_like, &mut other] {
        group.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let mut groups = Vec::new();
    if !other.is_empty() {
        groups.push(("config", other));
    }
    if !path_like.is_empty() {
        groups.push(("paths", path_like));
    }
    if !sensitive.is_empty() {
        groups.push(("secrets (masked)", sensitive));
    }
    groups
}

/// Environment of a process, as a map for lookups.
pub fn environment_map(pid: u32) -> Result<HashMap<String, String>> {
    Ok(environment(pid)?.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `sample` report of a fish shell waiting on input.
    const REPORT: &str = "\
Analysis of sampling fish (pid 15916) every 1 millisecond
Process:         fish [15916]

Call graph:
    891 Thread_4453802   DispatchQueue_1: com.apple.main-thread  (serial)
      891 start  (in dyld) + 6992  [0x184a9be00]
        891 main  (in fish) + 5016  [0x1043eff74]
          891 reader_read(parser_t&, int)  (in fish) + 2236  [0x10448b7f8]
            891 topic_monitor_t::check(generation_list_t*, bool)  (in fish) + 40  [0x1044a]
              891 binary_semaphore_t::wait()  (in fish) + 116  [0x1044b]
                891 read  (in libsystem_kernel.dylib) + 8  [0x184e16]
    12 Thread_4453999
      12 _pthread_start  (in libsystem_pthread.dylib) + 136  [0x184e5]
        12 my_worker_loop  (in fish) + 44  [0x10449]

Binary Images:
       0x1043ec000 -        0x104â€¦ fish
";

    #[test]
    fn the_leaf_frame_is_what_the_thread_was_actually_doing() {
        let threads = parse_call_graph(REPORT);
        assert_eq!(threads.len(), 2);
        let main = &threads[0];
        assert_eq!(
            main.label,
            "DispatchQueue_1: com.apple.main-thread  (serial)"
        );
        assert_eq!(main.samples, 891);
        assert_eq!(main.leaf, "read  (in libsystem_kernel.dylib)");
    }

    #[test]
    fn a_kernel_wait_is_distinguished_from_real_work() {
        let threads = parse_call_graph(REPORT);
        // Parked in read(2): idle, not busy. Reporting these the same way would
        // invert the answer the facet exists to give.
        assert!(threads[0].is_waiting());
        // A worker loop in the process's own code is not a wait.
        assert!(!threads[1].is_waiting(), "leaf: {}", threads[1].leaf);
    }

    #[test]
    fn stack_context_runs_innermost_first_and_is_bounded() {
        let threads = parse_call_graph(REPORT);
        let stack = &threads[0].stack;
        assert_eq!(stack[0], "read  (in libsystem_kernel.dylib)");
        assert!(stack.len() <= STACK_CONTEXT + 1);
        // The caller of the leaf comes next, not the outermost frame.
        assert!(stack[1].contains("binary_semaphore_t::wait"));
    }

    #[test]
    fn frames_drop_addresses_and_offsets() {
        assert_eq!(
            clean_frame("main  (in fish) + 5016  [0x1043eff74]"),
            "main  (in fish)"
        );
        // A symbol containing ` + ` that is not an offset survives.
        assert_eq!(
            clean_frame("operator + (int)  (in x)"),
            "operator + (int)  (in x)"
        );
        // A deduplicated symbol lists several offsets at once.
        assert_eq!(
            clean_frame("<deduplicated_symbol>  (in libsystem_malloc.dylib) + 0,136"),
            "<deduplicated_symbol>  (in libsystem_malloc.dylib)"
        );
    }

    #[test]
    fn threads_are_ordered_by_sample_count() {
        let threads = parse_call_graph(REPORT);
        assert!(threads[0].samples >= threads[1].samples);
    }

    #[test]
    fn a_report_without_a_call_graph_yields_nothing() {
        assert!(parse_call_graph("Process: x [1]\nBinary Images:\n").is_empty());
    }

    /// Real `ps eww` output: command line first, then the environment, with no
    /// delimiter between them.
    const PS_EWW: &str = "/opt/homebrew/bin/fish --login ATUIN_SESSION=019fa6 \
                          COLORTERM=truecolor EDITOR=/opt/homebrew/bin/nvim \
                          LS_COLORS=di=1;36:ln=35 GITHUB_TOKEN=ghp_abcdefghij";

    #[test]
    fn the_environment_is_split_from_the_command_line() {
        let vars = parse_environment(PS_EWW);
        let keys: Vec<&str> = vars.iter().map(|(key, _)| key.as_str()).collect();
        assert!(keys.contains(&"ATUIN_SESSION"));
        assert!(keys.contains(&"EDITOR"));
        // `--login` is a command-line token, not a variable.
        assert!(!keys.iter().any(|key| key.contains("login")));
    }

    #[test]
    fn a_flag_containing_equals_is_not_mistaken_for_a_variable() {
        // The real case: claude is started with --settings={"a":1}.
        let raw = "claude --model default --settings={\"enabledPlugins\":true} HOME=/Users/g";
        let vars = parse_environment(raw);
        let keys: Vec<&str> = vars.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, vec!["HOME"], "got: {keys:?}");
    }

    #[test]
    fn a_value_containing_spaces_stays_with_its_key() {
        let vars = parse_environment("cmd MESSAGE=hello there world NEXT=1");
        let map: HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(map["MESSAGE"], "hello there world");
        assert_eq!(map["NEXT"], "1");
    }

    #[test]
    fn secret_looking_values_are_masked_but_their_names_are_kept() {
        // Knowing a token is *set* is usually the question; its value is a leak.
        assert!(is_sensitive("GITHUB_TOKEN"));
        assert!(is_sensitive("AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive("ATUIN_SESSION"));
        assert!(!is_sensitive("EDITOR"));

        let masked = display_value("GITHUB_TOKEN", "ghp_abcdefghij");
        assert!(!masked.contains("ghp_"), "{masked}");
        assert!(masked.contains("14 chars"));
        // An unset-but-present variable reads as empty, not as a fake mask.
        assert_eq!(display_value("GITHUB_TOKEN", ""), "");
        assert_eq!(display_value("EDITOR", "nvim"), "nvim");
    }

    #[test]
    fn grouping_puts_config_first_and_secrets_last() {
        let vars = parse_environment(PS_EWW);
        let groups = grouped(&vars);
        let names: Vec<&str> = groups.iter().map(|(name, _)| *name).collect();
        assert_eq!(names.first(), Some(&"config"));
        assert_eq!(names.last(), Some(&"secrets (masked)"));

        let secrets = &groups.last().unwrap().1;
        assert!(secrets.iter().any(|(key, _)| key == "GITHUB_TOKEN"));
    }

    /// Real `sample` output for a Node-based binary: branch characters (`+`, `!`),
    /// unsymbolized `???` frames, and a fork where one child holds 860 samples and
    /// its sibling 2.
    const BRANCHED: &str = "\
Call graph:
    862 Thread_30074   DispatchQueue_1: com.apple.main-thread  (serial)
    + 862 start  (in dyld) + 6992  [0x184a9be00]
    +   862 ???  (in claude.exe)  load address 0x104b3c000 + 0xa28354  [0x105564354]
    +     862 ???  (in claude.exe)  load address 0x104b3c000 + 0x10ab0c8  [0x105be70c8]
    +       860 ???  (in claude.exe)  load address 0x104b3c000 + 0x7f9200  [0x105335200]
    +       ! 860 kevent64  (in libsystem_kernel.dylib) + 8  [0x184e21ba8]
    +       2 ???  (in claude.exe)  load address 0x104b3c000 + 0x7f9088  [0x105335088]
    +         2 rare_side_path  (in claude.exe) + 4  [0x1073f425c]

Binary Images:
";

    #[test]
    fn branch_characters_do_not_break_the_sample_count() {
        let threads = parse_call_graph(BRANCHED);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].samples, 862);
    }

    #[test]
    fn a_forked_stack_follows_the_heavy_branch_not_the_last_line() {
        // The 2-sample branch is printed last. Taking the final line as the leaf
        // reported a rare path as if it were what the thread does.
        let threads = parse_call_graph(BRANCHED);
        assert_eq!(threads[0].leaf, "kevent64  (in libsystem_kernel.dylib)");
        assert!(
            !threads[0]
                .stack
                .iter()
                .any(|f| f.contains("rare_side_path")),
            "stack: {:?}",
            threads[0].stack
        );
    }

    #[test]
    fn a_kevent_wait_is_recognised_as_idle() {
        // This is the case that reported three threads as BUSY with a blank leaf.
        let threads = parse_call_graph(BRANCHED);
        assert!(!threads[0].leaf.is_empty(), "leaf must never be blank");
        assert!(threads[0].is_waiting(), "leaf: {}", threads[0].leaf);
    }

    #[test]
    fn unsymbolized_frames_keep_the_binary_name() {
        // `???  (in claude.exe)  load address 0x… + 0x…` is all noise but the name.
        assert_eq!(
            clean_frame("???  (in claude.exe)  load address 0x104b3c000 + 0xa28354  [0x105564354]"),
            "[claude.exe]"
        );
        let threads = parse_call_graph(BRANCHED);
        assert!(
            threads[0].stack.iter().any(|f| f == "[claude.exe]"),
            "stack: {:?}",
            threads[0].stack
        );
    }

    #[test]
    fn kevent64_variants_count_as_waits() {
        // The wait list must match the real symbol, which carries a suffix.
        let sample = ThreadSample {
            label: "x".into(),
            samples: 1,
            leaf: "kevent64  (in libsystem_kernel.dylib)".into(),
            stack: vec![],
        };
        assert!(sample.is_waiting());
    }

    #[test]
    fn lowercase_variable_names_are_recognised() {
        // Real case: a tmux config exports `color_black=colour232`. Rejecting
        // lowercase names folded them into the previous variable's value.
        let vars = parse_environment("cmd COMMAND_MODE=unix2003 color_black=colour232 X=1");
        let map: HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(map["COMMAND_MODE"], "unix2003");
        assert_eq!(map["color_black"], "colour232");
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn a_key_that_is_not_a_shell_identifier_is_not_a_variable() {
        // These all appear inside real command lines.
        assert!(split_env_token("--settings={\"a\":1}").is_none());
        assert!(split_env_token("-o=x").is_none());
        assert!(
            split_env_token("2FOO=x").is_none(),
            "cannot lead with a digit"
        );
        assert!(split_env_token("a.b=x").is_none());
        assert!(split_env_token("path/to=x").is_none());
        // But these are legal.
        assert!(split_env_token("_UNDERSCORE=x").is_some());
        assert!(split_env_token("VAR2=x").is_some());
    }
}
