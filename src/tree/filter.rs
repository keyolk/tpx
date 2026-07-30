//! Row filtering: the fuzzy matcher and the ancestor-retention pass.
//!
//! Split from `tree.rs` because the matcher has no dependency on how rows are
//! built — it only needs a [`Row`] to inspect.

use super::{Kind, Row};

/// Keep every row that matches, plus each match's ancestor chain.
///
/// A matched process shown without its window and pane is unusable — the whole
/// point of finding it is learning *where* it lives. Rows arrive in depth-first
/// order, so one reverse pass suffices: walking from the bottom, a row is kept
/// when it matches, or when a deeper row already kept is below it.
pub(super) fn retain_matches_with_ancestors(rows: Vec<Row>, filter: &Filter) -> Vec<Row> {
    let mut kept: Vec<Row> = Vec::new();
    // Depth of the shallowest kept descendant seen so far. Any row shallower
    // than this is an ancestor of a match and must be kept for context.
    let mut needed_depth: Option<u16> = None;

    for row in rows.into_iter().rev() {
        let is_ancestor_of_match = needed_depth.is_some_and(|depth| row.depth < depth);
        if filter.matches_row(&row) || is_ancestor_of_match {
            needed_depth = Some(row.depth);
            kept.push(row);
        }
    }
    kept.reverse();
    kept
}

/// Fuzzy row filter. Space-separated tokens must all match (AND), each either
/// as a substring or as a subsequence — the a9s convention, so muscle memory
/// carries over.
#[derive(Default, Clone)]
pub struct Filter {
    pub query: String,
}

impl Filter {
    pub fn is_active(&self) -> bool {
        !self.query.trim().is_empty()
    }

    pub fn matches_row(&self, row: &Row) -> bool {
        if !self.is_active() {
            return true;
        }
        // Match against everything the row makes visible, so `/8080` finds a
        // process by its port and `/shop` finds it by cwd.
        let mut haystack = row.label();
        match &row.kind {
            Kind::Process { proc } => {
                haystack.push(' ');
                haystack.push_str(&proc.command);
                // The pid is the identifier a reader arrives with — from a log,
                // a crash report, or `ps` in another pane — so `/90814` must
                // find that process rather than matching digits in a path.
                haystack.push(' ');
                haystack.push_str(&proc.key.pid.to_string());
            }
            Kind::Pane { pane } => {
                haystack.push(' ');
                haystack.push_str(&pane.cwd);
                haystack.push(' ');
                haystack.push_str(&pane.current_command);
            }
            Kind::Container { container } => {
                haystack.push(' ');
                haystack.push_str(&container.image);
                // The row shows a shortened name, but the filter matches the
                // full one — a pod name typed from `kubectl` output must find
                // its container even though the row elides it.
                haystack.push(' ');
                haystack.push_str(&container.name);
                if let Some(project) = &container.compose_project {
                    haystack.push(' ');
                    haystack.push_str(project);
                }
            }
            _ => {}
        }
        for port in &row.listen_ports {
            haystack.push_str(&format!(" {port}"));
        }
        fuzzy_matches(&self.query, &haystack)
    }
}

/// Every whitespace-separated token must match, as substring or subsequence.
fn fuzzy_matches(query: &str, candidate: &str) -> bool {
    let candidate = candidate.to_lowercase();
    query
        .split_whitespace()
        .all(|token| matches_token(&token.to_lowercase(), &candidate))
}

/// Longest token a subsequence match is allowed for.
///
/// Subsequence matching is what makes `crgo` find `cargo`, but its false-positive
/// rate grows with token length against long haystacks: `claude` matches
/// `lo**c**a**l**-path-provisioner-5**d**b9d5cbbb-dbx4n` purely by accident.
/// Short tokens are plausibly abbreviations; long ones are names the reader
/// typed in full and expects to match as such.
const MAX_SUBSEQUENCE_TOKEN: usize = 5;

fn matches_token(token: &str, candidate: &str) -> bool {
    if candidate.contains(token) {
        return true;
    }
    if token.chars().count() > MAX_SUBSEQUENCE_TOKEN {
        return false;
    }
    // Anchor at a word boundary so the abbreviation reads as initials or a
    // prefix run, not scattered letters: `crgo` → `cargo`, but not `claude` →
    // `local-path-...`.
    let mut rest = candidate;
    for ch in token.chars() {
        match rest.find(ch) {
            Some(at) => rest = &rest[at + ch.len_utf8()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_token_matching_is_and_across_tokens() {
        assert!(fuzzy_matches("cargo run", "cargo run --release"));
        assert!(!fuzzy_matches("cargo missing", "cargo run --release"));
        // Subsequence, not just substring.
        assert!(fuzzy_matches("crgo", "cargo run"));
    }

    #[test]
    fn a_long_token_does_not_match_by_scattered_letters() {
        // Observed false positive: `claude` matched
        // `local-path-provisioner-5db9d5cbbb-dbx4n` as a subsequence.
        assert!(!fuzzy_matches(
            "claude",
            "local-path-provisioner-5db9d5cbbb-dbx4n"
        ));
        assert!(!fuzzy_matches(
            "claude",
            "buildx_buildkit_multiarchbuilder0"
        ));
        // But a real substring still matches.
        assert!(fuzzy_matches("claude", "/opt/claude/bin/claude --resume"));
    }

    #[test]
    fn short_abbreviations_still_work_as_subsequences() {
        assert!(fuzzy_matches("crgo", "cargo build"));
        assert!(fuzzy_matches("nvm", "nvim src/main.rs"));
    }
}
