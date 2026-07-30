//! Semantic color tokens. Render paths reference these, never a raw color —
//! one place to change, and the whole app degrades together.
//!
//! ANSI 16-color only. It respects the terminal theme and downgrades cleanly to
//! monochrome, which truecolor literals do not.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Magenta;
pub const INFO: Color = Color::Cyan;
pub const SUCCESS: Color = Color::LightGreen;
pub const WARN: Color = Color::Yellow;
pub const ERROR: Color = Color::LightRed;
pub const DIM: Color = Color::DarkGray;
pub const BORDER: Color = Color::DarkGray;
pub const BORDER_FOCUS: Color = Color::Cyan;

/// Whether color is suppressed. Read once at startup; `NO_COLOR` being set at
/// all (even empty) disables color, per the no-color.org convention.
pub fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// Style builder that honors [`no_color`]. Modifiers (bold, reverse, dim) still
/// apply — they carry hierarchy when hue cannot.
#[derive(Clone, Copy)]
pub struct Palette {
    monochrome: bool,
}

impl Palette {
    pub fn new() -> Self {
        Self {
            monochrome: no_color(),
        }
    }

    pub fn fg(self, color: Color) -> Style {
        if self.monochrome {
            Style::default()
        } else {
            Style::default().fg(color)
        }
    }

    pub fn dim(self) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(DIM)
        }
    }

    pub fn bold(self, color: Color) -> Style {
        self.fg(color).add_modifier(Modifier::BOLD)
    }

    /// Selection. Reverse video works in monochrome, so it is the primary
    /// selection signal and color is only an accent on top.
    pub fn selected(self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    }

    pub fn border(self, focused: bool) -> Style {
        self.fg(if focused { BORDER_FOCUS } else { BORDER })
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

/// Severity of a metric, so the same thresholds color cpu everywhere.
pub fn cpu_style(palette: Palette, cpu_pct: f32) -> Style {
    match cpu_pct {
        cpu if cpu >= 80.0 => palette.fg(ERROR),
        cpu if cpu >= 20.0 => palette.fg(WARN),
        cpu if cpu >= 1.0 => palette.fg(SUCCESS),
        _ => palette.dim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_palette_emits_no_foreground_color() {
        let palette = Palette { monochrome: true };
        assert_eq!(palette.fg(ERROR), Style::default());
        // Hierarchy survives via modifiers.
        assert!(palette.bold(ERROR).add_modifier.contains(Modifier::BOLD));
        assert!(palette.dim().add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn selection_uses_reverse_video_so_it_works_without_color() {
        let palette = Palette { monochrome: true };
        assert!(palette.selected().add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn cpu_thresholds_escalate() {
        let palette = Palette { monochrome: false };
        assert_eq!(cpu_style(palette, 0.0), palette.dim());
        assert_eq!(cpu_style(palette, 5.0).fg, Some(SUCCESS));
        assert_eq!(cpu_style(palette, 45.0).fg, Some(WARN));
        assert_eq!(cpu_style(palette, 150.0).fg, Some(ERROR));
    }
}
