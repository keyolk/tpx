//! Rendering. Layout is computed from the frame area every draw, so resize is
//! automatic; nothing is a fixed width.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::model::{Origin, human_age, human_bytes};
use crate::palette::{self, ACCENT, ERROR, INFO, Palette, SUCCESS, WARN};
use crate::tree::{Kind, Row, Scope, Sort};

use super::app::{App, Facet, Modal};
use super::keys::{FOOTER_HINTS, KEYMAP};

/// Below this the layout cannot show a tree and a detail pane at once.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
/// Below this the detail pane is dropped and the tree gets the whole width — a
/// 60-column tmux split is a real place this runs.
const SPLIT_WIDTH: u16 = 90;
/// Narrowest the detail pane may be while still readable: an established-socket
/// line (`tcp  10.0.0.5:443 → 10.0.0.9:51234`) is the widest thing it must fit.
const DETAIL_MIN_WIDTH: u16 = 40;
/// Widest the tree's content is allowed to be. On a very wide terminal a
/// full-width row strands the right-aligned metrics ~100 columns from the label
/// they describe, and the eye cannot connect them; past this the metrics column
/// stays put and the empty space goes to the detail pane.
const TREE_CONTENT_MAX: u16 = 110;

const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
/// ASCII fallback: the spinner is the only animated glyph, and a terminal that
/// cannot render braille would show boxes on every frame.
const SPINNER_ASCII: [&str; 4] = ["|", "/", "-", "\\"];

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }

    let palette = Palette::new();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, chunks[0], app, palette);
    render_body(frame, chunks[1], app, palette);
    render_footer(frame, chunks[2], app, palette);

    match &app.modal {
        Some(Modal::Help) => render_help(frame, area, palette),
        Some(Modal::Diagnostics) => render_diagnostics(frame, area, app, palette),
        Some(Modal::Confirm { title, command, .. }) => {
            render_confirm(frame, area, title, command, palette)
        }
        Some(Modal::CommandMenu) => render_command_menu(frame, area, palette),
        Some(Modal::SortMenu) => render_sort_menu(frame, area, app, palette),
        None => {}
    }
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let message = Paragraph::new(vec![
        Line::from("terminal too small"),
        Line::from(format!(
            "need {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
            area.width, area.height
        )),
    ])
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(message, area);
}

fn render_body(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    // Narrow terminals drop the detail pane rather than squeezing both into
    // unreadable columns; the facet is still reachable by widening.
    if area.width < SPLIT_WIDTH {
        render_tree(frame, area, app, palette);
        return;
    }
    // Reserve 1 column for the vertical separator, then cap the tree at the
    // width its content uses. Without reserving the separator, the tree's
    // right-aligned metrics overlapped it.
    let tree_width = (area.width.saturating_sub(DETAIL_MIN_WIDTH + 1)).min(TREE_CONTENT_MAX + 2);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(tree_width),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
    render_tree(frame, panes[0], app, palette);

    // A single-column vertical separator — cheaper than a full box border, and
    // it does not enclose the empty rows below the content the way Borders::ALL
    // did.
    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(palette.border(false)),
        panes[1],
    );

    render_detail(frame, panes[2], app, palette);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let mut spans = vec![Span::styled("tpx", palette.bold(ACCENT))];

    // The scope leads, because a narrowed tree that does not say so reads as
    // "this is everything" — and by default it is one window out of many.
    match (&app.scope, &app.current_window) {
        (Scope::CurrentWindow, Some((session, window))) => {
            spans.push(Span::styled(
                format!("  {session}:{window}"),
                palette.bold(INFO),
            ));
            spans.push(Span::styled(" (w for server)", palette.dim()));
        }
        (Scope::CurrentWindow, None) => {
            // Outside tmux there is no window to narrow to, so everything shows.
            spans.push(Span::styled("  whole server", palette.bold(INFO)));
            spans.push(Span::styled(" (not inside tmux)", palette.dim()));
        }
        (Scope::Server, _) => {
            spans.push(Span::styled("  whole server", palette.bold(INFO)));
        }
    }

    // Counts describe what is *rendered*, not what was collected — the snapshot
    // is always server-wide, so snapshot counts would contradict the tree.
    //
    // A flat ordering has no pane rows to sum, so processes are counted directly;
    // deriving them from pane rollups would report zero.
    if app.sort.is_flat() {
        let procs = app
            .rows
            .iter()
            .filter(|row| matches!(row.kind, Kind::Process { .. }))
            .count();
        spans.push(Span::styled(format!("  {procs} procs"), palette.dim()));
    } else {
        let panes = app
            .rows
            .iter()
            .filter(|row| matches!(row.kind, Kind::Pane { .. }))
            .count();
        let procs: u32 = app
            .rows
            .iter()
            .filter(|row| matches!(row.kind, Kind::Pane { .. }))
            .map(|row| row.rollup.proc_count)
            .sum();
        spans.push(Span::styled(
            format!("  {panes} panes · {procs} procs"),
            palette.dim(),
        ));
    }

    if app.docker_available {
        let running = app.snapshot.containers.iter().filter(|c| c.running).count();
        spans.push(Span::styled(
            format!(" · {running}/{} containers", app.snapshot.containers.len()),
            palette.dim(),
        ));
    }

    // A flat list looks like a broken tree until the reader knows it is sorted, so
    // the ordering is named whenever it is not the default.
    if app.sort != Sort::Tree {
        spans.push(Span::styled(
            format!("  ↓{}", app.sort.label()),
            palette.bold(WARN),
        ));
    }

    // A filter changes what the tree means, so it is stated in the header, not
    // just in the footer where it could be missed.
    if app.filter.is_active() || app.filter_input.is_some() {
        let query = app.filter_input.as_deref().unwrap_or(&app.filter.query);
        let editing = app.filter_input.is_some();
        spans.push(Span::styled(
            format!("  /{query}{}", if editing { "▏" } else { "" }),
            palette.fg(if editing { INFO } else { WARN }),
        ));
    }

    if app.collector.in_flight {
        spans.push(Span::styled(
            format!("  {} refreshing", spinner(app)),
            palette.fg(INFO),
        ));
    }
    if !app.snapshot.errors.is_empty() {
        // Never let a failed collector look like an empty world.
        spans.push(Span::styled(
            format!(
                "  ! {} collector error(s) — press !",
                app.snapshot.errors.len()
            ),
            palette.fg(ERROR),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn spinner(app: &App) -> &'static str {
    let frames = if supports_unicode() {
        SPINNER
    } else {
        SPINNER_ASCII
    };
    frames[app.spinner % frames.len()]
}

/// Whether to use box-drawing and braille glyphs. `TPX_ASCII=1` forces the
/// fallback for terminals that render them as boxes.
fn supports_unicode() -> bool {
    std::env::var_os("TPX_ASCII").is_none()
}

fn render_tree(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let focused = app.modal.is_none();
    // A border around the whole area draws 40+ empty rows when there are 8
    // processes. The title alone identifies the pane; a focused border style
    // marks which side is active without enclosing empty space.
    let title = Span::styled(
        " tree ",
        palette.fg(if focused { INFO } else { Color::Reset }),
    );
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(palette.border(focused))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.rows.is_empty() {
        let message = if app.filter.is_active() {
            "no rows match the filter"
        } else if app.snapshot.panes.is_empty() {
            "no tmux panes found — is a tmux server running?"
        } else {
            "collecting…"
        };
        frame.render_widget(Paragraph::new(message).style(palette.dim()), inner);
        return;
    }

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            tree_row(
                row,
                inner.width.min(TREE_CONTENT_MAX),
                palette,
                app.sort.is_flat(),
            )
        })
        .collect();
    let list = List::new(items).highlight_style(palette.selected());

    // ratatui owns the scroll offset: it adjusts whatever it is given so the
    // selection stays on screen. Feeding back a stored offset fought that
    // adjustment — after a rebuild the stored value referred to a different row
    // count, and the view jumped to the top. Letting ratatui derive the offset
    // from the selection each frame keeps the selected row visible, which is the
    // property that actually matters.
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, inner, &mut state);
}

fn tree_row(row: &Row, width: u16, palette: Palette, flat: bool) -> ListItem<'static> {
    let mut spans = Vec::new();

    // Indent + fold marker. The marker column is always present so labels line
    // up whether or not a row is expandable.
    let indent = "  ".repeat(row.depth as usize);
    let marker = match (row.expandable, row.expanded) {
        (true, true) => {
            if supports_unicode() {
                "▾ "
            } else {
                "- "
            }
        }
        (true, false) => {
            if supports_unicode() {
                "▸ "
            } else {
                "+ "
            }
        }
        (false, _) => "  ",
    };
    spans.push(Span::styled(format!("{indent}{marker}"), palette.dim()));

    match &row.kind {
        Kind::Session {
            name,
            attached,
            window_count,
        } => {
            spans.push(Span::styled(name.clone(), palette.bold(ACCENT)));
            spans.push(Span::styled(
                format!(
                    "  {window_count}w{}",
                    if *attached { " ·attached" } else { "" }
                ),
                palette.dim(),
            ));
        }
        Kind::Window {
            name,
            index,
            active,
            pane_count,
            zoomed,
        } => {
            let style = if *active {
                palette.bold(INFO)
            } else {
                palette.fg(INFO)
            };
            spans.push(Span::styled(format!("{index}:{name}"), style));
            spans.push(Span::styled(format!("  {pane_count}p"), palette.dim()));
            if *zoomed {
                // Zoom is a window property; showing it on each pane row would
                // repeat the same fact `pane_count` times.
                spans.push(Span::styled("  Z", palette.fg(WARN)));
            }
        }
        Kind::Pane { pane } => {
            let style = if pane.active {
                palette.bold(SUCCESS)
            } else {
                palette.fg(SUCCESS)
            };
            spans.push(Span::styled(
                format!("{}.{}", pane.window_index, pane.pane_index),
                style,
            ));
            spans.push(Span::styled(
                format!("  {}", shorten_path(&pane.cwd)),
                palette.dim(),
            ));
        }
        Kind::Process { proc } => {
            spans.push(Span::raw(proc.name().to_string()));
            spans.push(Span::styled(format!("  {}", proc.key.pid), palette.dim()));
            // A Claude Code process gets its session id shown inline — it is the
            // identifier the reader uses to find a session in `ccx` or logs.
            if matches!(proc.key.origin, Origin::Host)
                && crate::collect::claude::is_claude(&proc.command)
                && let Some(cwd) = &row.pane_cwd
                && let Some(session) = crate::collect::claude::session_for(cwd)
            {
                let short = &session.session_id[..session.session_id.len().min(8)];
                spans.push(Span::styled(format!(" ⟡{short}"), palette.fg(ACCENT)));
            }
            // In a flat ordering the indent is gone, so the pane that owned this
            // process is spelled out — otherwise the row says what is heavy without
            // saying where it lives.
            if let Some(pane) = &row.flat_context {
                spans.push(Span::styled(format!("  @{pane}"), palette.fg(SUCCESS)));
            }
            // A container-namespace pid is marked, because pid 7 in a container
            // and pid 7 on the host are unrelated and confusing side by side.
            if matches!(proc.key.origin, Origin::Container(_)) {
                spans.push(Span::styled("  ⧉", palette.fg(INFO)));
            }
            if proc.state.starts_with('Z') {
                spans.push(Span::styled("  Z-zombie", palette.fg(ERROR)));
            }
        }
        Kind::Container { container } => {
            spans.push(Span::styled(container.display_name(), palette.fg(WARN)));
            let mark = if container.running { "" } else { " ·stopped" };
            spans.push(Span::styled(
                format!("  {}{mark}", container.display_image()),
                palette.dim(),
            ));
            if let Some(attribution) = &container.attribution {
                spans.push(Span::styled(
                    format!("  ({})", attribution.reason.label()),
                    palette.dim(),
                ));
            }
        }
    }

    // Right-aligned metrics, padded to the pane width. Cell width, not char
    // count — a CJK path in a cwd would otherwise misalign every row.
    let metrics = row_metrics(row, flat);
    let left_width: usize = spans.iter().map(|span| span.content.width()).sum();
    let metrics_width = metrics
        .iter()
        .map(|span| span.content.width())
        .sum::<usize>();
    let available = width as usize;
    if left_width + metrics_width + 1 < available {
        spans.push(Span::raw(
            " ".repeat(available - left_width - metrics_width),
        ));
        spans.extend(metrics);
    }

    ListItem::new(Line::from(spans))
}

/// The metric cluster: cpu, memory, ports, connections, age.
///
/// Every field is printed at a fixed width — even when zero — so the columns
/// line up across rows. Conditional printing left blank cells that pulled the
/// next field left, making a glanceable table read as unsorted noise.
fn row_metrics(row: &Row, flat: bool) -> Vec<Span<'static>> {
    let palette = Palette::new();

    // In a flat ordering the row must show the value it was *sorted by*. Showing
    // the subtree rollup instead made a cpu-sorted list read as unsorted: a shell
    // whose child burned 13% displayed 13% while ranking on its own 0%.
    let (cpu, rss) = if flat {
        (row.own_cpu(), row.own_rss())
    } else {
        (row.rollup.cpu_pct, row.rollup.rss_bytes)
    };

    let mut spans = Vec::new();

    // cpu — fixed 6 cells. The %cpu suffix is dropped to save width; cpu is the
    // leftmost metric and is self-evidently cpu.
    spans.push(Span::styled(
        format!("{cpu:>5.1}% "),
        palette::cpu_style(palette, cpu),
    ));
    // rss — fixed 7 cells.
    spans.push(Span::styled(
        format!("{:>6} ", human_bytes(rss)),
        palette.dim(),
    ));
    if !row.listen_ports.is_empty() {
        let ports: Vec<String> = row
            .listen_ports
            .iter()
            .take(3)
            .map(u16::to_string)
            .collect();
        let more = if row.listen_ports.len() > 3 { "+" } else { "" };
        let style = if row.port_conflict {
            palette.fg(ERROR)
        } else {
            palette.fg(SUCCESS)
        };
        // `!` marks the conflict in text, not only in red.
        let conflict = if row.port_conflict { "!" } else { "" };
        spans.push(Span::styled(
            format!("L:{}{more}{conflict} ", ports.join(",")),
            style,
        ));
    } else {
        // Reserve the same width a single port would take so the next column does
        // not jump left on rows without listeners.
        spans.push(Span::raw("       "));
    }
    if row.connections > 0 {
        spans.push(Span::styled(
            format!("E:{} ", row.connections),
            palette.fg(INFO),
        ));
    } else {
        spans.push(Span::raw("    "));
    }
    // age — only for processes, but the column is part of the row so group rows
    // get spaces to keep the table aligned.
    if let Kind::Process { proc } = &row.kind {
        spans.push(Span::styled(
            format!("{:>5}", human_age(proc.age_secs)),
            palette.dim(),
        ));
    } else {
        spans.push(Span::raw("     "));
    }
    spans
}

fn render_detail(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    // No full border: same reason as the tree — a 40-row box around 7 rows of
    // content is mostly empty chrome. A top border + title is enough to mark
    // the pane, and the content scrolls below it.
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(palette.border(false))
        .title(Span::styled(" detail ", palette.dim()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    // Facet tabs. The active tab is the only one highlighted, so the pane's
    // identity is always visible without moving anything.
    let mut tabs = Vec::new();
    for (index, facet) in Facet::ALL.iter().enumerate() {
        let active = *facet == app.facet;
        let style = if active {
            palette.bold(INFO)
        } else {
            palette.dim()
        };
        tabs.push(Span::styled(
            format!(" {}·{} ", index + 1, facet.title()),
            style,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), chunks[0]);

    let lines = super::facets::lines_for(app, app.facet, palette);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);
}

pub(super) fn field(label: &str, value: String, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), palette.dim()),
        Span::raw(value),
    ])
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    if let Some(status) = &app.status {
        let style = if status.is_error {
            palette.fg(ERROR)
        } else {
            palette.fg(SUCCESS)
        };
        frame.render_widget(
            Paragraph::new(Line::styled(status.message.clone(), style)),
            area,
        );
        return;
    }

    let mut spans = Vec::new();
    for (key, action) in FOOTER_HINTS {
        spans.push(Span::styled(format!(" {key}"), palette.bold(INFO)));
        spans.push(Span::styled(format!(":{action}"), palette.dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Centered overlay box, sized as a fraction of the frame but never larger than
/// it — a fixed size would overflow a small terminal.
fn overlay(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = (area.width * width_percent / 100).min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn render_help(frame: &mut Frame, area: Rect, palette: Palette) {
    // +4 for the border, the blank line and the close hint. The overlay clamps
    // to the frame, so on a short terminal the box shrinks and the content
    // scrolls rather than spilling past the bottom edge.
    let box_area = overlay(area, 70, KEYMAP.len() as u16 + 4);
    frame.render_widget(Clear, box_area);

    let mut lines = Vec::new();
    for (key, action) in KEYMAP {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<10}"), palette.bold(INFO)),
            Span::raw(*action),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::styled("  Esc / q to close", palette.dim()));

    // The close hint is the one line that must always be visible: a reader who
    // cannot see how to leave is stuck.
    let scroll = scroll_to_end(&lines, box_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(palette.fg(ACCENT))
        .title(Span::styled(" keys ", palette.bold(ACCENT)));
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        box_area,
    );
}

fn render_diagnostics(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let conflicts = app.snapshot.port_conflicts();
    let height = (app.snapshot.errors.len() + conflicts.len() + 8) as u16;
    let box_area = overlay(area, 80, height);
    frame.render_widget(Clear, box_area);

    let mut lines = vec![Line::styled("  collectors", palette.dim())];
    if app.snapshot.errors.is_empty() {
        lines.push(Line::styled("    all ok", palette.fg(SUCCESS)));
    }
    for error in &app.snapshot.errors {
        lines.push(Line::from(vec![
            Span::styled(format!("    {:<8} ", error.source), palette.fg(ERROR)),
            Span::raw(error.message.clone()),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::styled("  contested listen ports", palette.dim()));
    if conflicts.is_empty() {
        lines.push(Line::styled("    none", palette.fg(SUCCESS)));
    }
    let mut ports: Vec<_> = conflicts.iter().collect();
    ports.sort_by_key(|(port, _)| **port);
    for (port, holders) in ports {
        let names: Vec<String> = holders
            .iter()
            .map(|key| {
                let name = app
                    .snapshot
                    .proc(key)
                    .map(|proc| proc.name().to_string())
                    .unwrap_or_else(|| "?".into());
                format!("{name}({})", key.pid)
            })
            .collect();
        lines.push(Line::from(vec![
            Span::styled(format!("    :{port:<6} "), palette.fg(WARN)),
            Span::raw(names.join(", ")),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::styled("  Esc / q to close", palette.dim()));

    let scroll = scroll_to_end(&lines, box_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(palette.fg(WARN))
        .title(Span::styled(" diagnostics ", palette.bold(WARN)));
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        box_area,
    );
}

/// Scroll offset that keeps the last line — always the "Esc to close" hint —
/// visible when the content is taller than the box.
fn scroll_to_end(lines: &[Line], box_area: Rect) -> u16 {
    let visible = box_area.height.saturating_sub(2) as usize;
    lines.len().saturating_sub(visible) as u16
}

fn render_confirm(frame: &mut Frame, area: Rect, title: &str, command: &str, palette: Palette) {
    let height = command.lines().count() as u16 + 7;
    let box_area = overlay(area, 80, height);
    frame.render_widget(Clear, box_area);

    let mut lines = vec![Line::raw(format!("  {title}")), Line::default()];
    // The exact command is shown so a privileged or side-effecting action is
    // never invisible.
    for line in command.lines() {
        lines.push(Line::styled(format!("  {line}"), palette.fg(INFO)));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  y", palette.bold(SUCCESS)),
        Span::raw(" run   "),
        Span::styled("Esc", palette.bold(ERROR)),
        Span::raw(" cancel"),
    ]));

    let scroll = scroll_to_end(&lines, box_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(palette.fg(WARN))
        .title(Span::styled(" confirm ", palette.bold(WARN)));
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        box_area,
    );
}

/// Replace `$HOME` with `~` and elide long middles. Paths dominate this UI, and
/// the interesting part is usually the tail.
pub(super) fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    const MAX: usize = 60;
    if path.width() <= MAX {
        return path;
    }
    // Keep the tail — it names the file; the middle of a long path rarely does.
    let tail: String = path
        .chars()
        .rev()
        .take(MAX - 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_path_uses_tilde_for_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        assert_eq!(shorten_path(&format!("{home}/src/app")), "~/src/app");
    }

    #[test]
    fn shorten_path_keeps_the_tail_of_a_long_path() {
        let long = format!("/a{}/final.log", "/very-long-directory-name".repeat(6));
        let short = shorten_path(&long);
        assert!(short.width() <= 60);
        assert!(short.ends_with("final.log"));
        assert!(short.starts_with("..."));
    }

    #[test]
    fn overlay_never_exceeds_the_frame() {
        let small = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        let box_area = overlay(small, 80, 100);
        assert!(box_area.width <= small.width);
        assert!(box_area.height <= small.height);
        assert!(box_area.y + box_area.height <= small.y + small.height);
    }

    #[test]
    fn overlay_is_centered() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let box_area = overlay(area, 50, 10);
        assert_eq!(box_area.x, 25);
        assert_eq!(box_area.y, 15);
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn a_tall_overlay_stays_inside_a_short_frame() {
        // 40-row terminal, 22-line keymap box: the box must not extend past the
        // bottom edge, or its close hint is unreachable.
        let frame = Rect {
            x: 0,
            y: 0,
            width: 190,
            height: 40,
        };
        let box_area = overlay(frame, 70, KEYMAP.len() as u16 + 4);
        assert!(
            box_area.y + box_area.height <= frame.height,
            "box spans rows {}..{} in a {}-row frame",
            box_area.y,
            box_area.y + box_area.height,
            frame.height
        );
    }

    #[test]
    fn overlay_taller_than_the_frame_is_clamped_to_it() {
        let frame = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 14,
        };
        let box_area = overlay(frame, 80, 100);
        assert!(box_area.height <= frame.height);
        assert!(box_area.y + box_area.height <= frame.height);
    }
}

#[cfg(test)]
mod metric_tests {
    use super::*;
    use crate::model::{Proc, ProcKey, Rollup};
    use crate::tree::NodeId;

    /// A shell with 0% of its own cpu whose subtree sums to 13%.
    fn shell_row() -> Row {
        Row {
            id: NodeId::Process(ProcKey::host(100)),
            kind: Kind::Process {
                proc: Proc {
                    key: ProcKey::host(100),
                    ppid: 1,
                    command: "fish".into(),
                    age_secs: 60,
                    cpu_pct: 0.0,
                    cpu_time_secs: 0.0,
                    rss_bytes: 17 * 1024 * 1024,
                    state: "S".into(),
                    threads: None,
                    fd_count: None,
                },
            },
            depth: 0,
            expandable: false,
            expanded: false,
            rollup: Rollup {
                proc_count: 3,
                cpu_pct: 13.6,
                rss_bytes: 849 * 1024 * 1024,
                listen_ports: 0,
            },
            listen_ports: vec![],
            port_conflict: false,
            connections: 0,
            flat_context: Some("local:1.2".into()),
            pane_cwd: None,
        }
    }

    fn rendered(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.to_string()).collect()
    }

    #[test]
    fn a_flat_row_shows_the_value_it_was_sorted_by() {
        // The bug: sorting used own_cpu (0%) while the row displayed the rollup
        // (13.6%), so a correctly sorted list read as unsorted.
        let flat = rendered(&row_metrics(&shell_row(), true));
        assert!(
            !flat.contains("13.6"),
            "flat row must not show subtree cpu: {flat}"
        );
        assert!(flat.contains("17"), "own rss expected: {flat}");
    }

    #[test]
    fn a_tree_row_still_shows_its_subtree_rollup() {
        // In the tree the rollup is the point: a collapsed node must account for
        // everything beneath it.
        let tree = rendered(&row_metrics(&shell_row(), false));
        assert!(tree.contains("13.6"), "tree row shows subtree cpu: {tree}");
        assert!(tree.contains("849"), "tree row shows subtree rss: {tree}");
    }
}

/// The `x` command menu — a compact popup listing two-keystroke commands.
fn render_command_menu(frame: &mut Frame, area: Rect, palette: Palette) {
    let entries = [
        ("xd", "dump packets"),
        ("xs", "stop capture"),
        ("xk", "send SIGTERM"),
        ("xo", "switch to pane"),
        ("xc", "copy selection"),
    ];
    let height = entries.len() as u16 + 4;
    let box_area = overlay(area, 40, height);
    frame.render_widget(Clear, box_area);

    let mut lines = vec![
        Line::styled("  x commands", palette.bold(ACCENT)),
        Line::default(),
    ];
    for (key, desc) in entries {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<4} "), palette.bold(INFO)),
            Span::raw(desc),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::styled("  Esc to cancel", palette.dim()));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(palette.fg(ACCENT))
        .title(Span::styled(" x ", palette.bold(ACCENT)));
    frame.render_widget(Paragraph::new(lines).block(block), box_area);
}

/// The sort submenu — pick an ordering directly instead of cycling.
fn render_sort_menu(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    use crate::tree::Sort;
    let entries = [
        ("1", Sort::Tree, "tmux order"),
        ("2", Sort::Cpu, "heaviest cpu first"),
        ("3", Sort::Memory, "largest rss first"),
        ("4", Sort::Age, "newest first"),
        ("5", Sort::Connections, "most connections first"),
    ];
    let height = entries.len() as u16 + 4;
    let box_area = overlay(area, 45, height);
    frame.render_widget(Clear, box_area);

    let mut lines = vec![
        Line::styled("  sort by", palette.bold(ACCENT)),
        Line::default(),
    ];
    for (key, sort, desc) in entries {
        let current = app.sort == sort;
        let marker = if current { "● " } else { "  " };
        let style = if current {
            palette.bold(SUCCESS)
        } else {
            palette.fg(INFO)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{key} "), style),
            Span::styled(format!("{:<12}", sort.label()), style),
            Span::styled(desc, palette.dim()),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::styled("  Esc to cancel", palette.dim()));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(palette.fg(ACCENT))
        .title(Span::styled(" sort ", palette.bold(ACCENT)));
    frame.render_widget(Paragraph::new(lines).block(block), box_area);
}
