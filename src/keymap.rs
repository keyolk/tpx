//! Physical-position key normalization for CJK input sources.
//!
//! With the OS input source set to Korean, pressing the `q` key emits `ㅂ`, so
//! every single-letter shortcut in the TUI silently stops working until the
//! user switches back to English. This module maps each jamo back to the Latin
//! key at the same physical position on a US QWERTY keyboard — the same idea as
//! Vim's `langmap`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Latin key at the same physical position as `ch` on the 2-set (두벌식) Korean
/// layout, or `None` when `ch` is not a jamo we map.
///
/// Shifted jamo (double consonants, ㅒ/ㅖ) map to the uppercase Latin letter,
/// which is what the same physical chord would have produced in English.
pub fn hangul_to_latin(ch: char) -> Option<char> {
    let latin = match ch {
        // unshifted row
        'ㅂ' => 'q',
        'ㅈ' => 'w',
        'ㄷ' => 'e',
        'ㄱ' => 'r',
        'ㅅ' => 't',
        'ㅛ' => 'y',
        'ㅕ' => 'u',
        'ㅑ' => 'i',
        'ㅐ' => 'o',
        'ㅔ' => 'p',
        'ㅁ' => 'a',
        'ㄴ' => 's',
        'ㅇ' => 'd',
        'ㄹ' => 'f',
        'ㅎ' => 'g',
        'ㅗ' => 'h',
        'ㅓ' => 'j',
        'ㅏ' => 'k',
        'ㅣ' => 'l',
        'ㅋ' => 'z',
        'ㅌ' => 'x',
        'ㅊ' => 'c',
        'ㅍ' => 'v',
        'ㅠ' => 'b',
        'ㅜ' => 'n',
        'ㅡ' => 'm',
        // shifted row
        'ㅃ' => 'Q',
        'ㅉ' => 'W',
        'ㄸ' => 'E',
        'ㄲ' => 'R',
        'ㅆ' => 'T',
        'ㅒ' => 'O',
        'ㅖ' => 'P',
        _ => return None,
    };
    Some(latin)
}

/// Rewrite a jamo key event to the Latin key at the same physical position.
///
/// Events carrying CTRL or ALT are returned untouched: those chords already
/// arrive as Latin regardless of the input source, and rewriting them would
/// break `ctrl+c` / `ctrl+u` style bindings.
pub fn normalize(key: KeyEvent) -> KeyEvent {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return key;
    }
    let KeyCode::Char(ch) = key.code else {
        return key;
    };
    let Some(latin) = hangul_to_latin(ch) else {
        return key;
    };
    // SHIFT is dropped along with the jamo: the uppercase Latin char already
    // carries the shift, and leaving the flag on would make `ㅃ` match a
    // `SHIFT+Q` arm that plain `Q` never sets.
    let mut out = KeyEvent::new(KeyCode::Char(latin), key.modifiers - KeyModifiers::SHIFT);
    out.kind = key.kind;
    out.state = key.state;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unshifted_jamo_map_to_lowercase_latin() {
        for (jamo, latin) in [('ㅂ', 'q'), ('ㅁ', 'a'), ('ㅋ', 'z'), ('ㅓ', 'j')] {
            assert_eq!(hangul_to_latin(jamo), Some(latin));
        }
    }

    #[test]
    fn shifted_jamo_map_to_uppercase_latin() {
        assert_eq!(hangul_to_latin('ㅃ'), Some('Q'));
        assert_eq!(hangul_to_latin('ㄲ'), Some('R'));
    }

    #[test]
    fn latin_and_composed_syllables_are_left_alone() {
        assert_eq!(hangul_to_latin('q'), None);
        assert_eq!(hangul_to_latin('가'), None);
    }

    #[test]
    fn normalize_rewrites_a_plain_jamo_press() {
        let key = KeyEvent::new(KeyCode::Char('ㅂ'), KeyModifiers::NONE);
        assert_eq!(normalize(key).code, KeyCode::Char('q'));
    }

    #[test]
    fn normalize_drops_shift_with_the_jamo() {
        let key = KeyEvent::new(KeyCode::Char('ㅃ'), KeyModifiers::SHIFT);
        let out = normalize(key);
        assert_eq!(out.code, KeyCode::Char('Q'));
        assert_eq!(out.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn normalize_leaves_control_chords_untouched() {
        let key = KeyEvent::new(KeyCode::Char('ㅁ'), KeyModifiers::CONTROL);
        assert_eq!(normalize(key), key);
    }
}
