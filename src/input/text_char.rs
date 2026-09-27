//! The character a plain key press types, for callers that model what a pane's input line holds.

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

use super::TerminalKey;

/// The character a press of `key` types into a text line: the character itself for an unmodified key, its shifted
/// form for a shifted one, and `None` for a release, a non-character key or any other modifier.
pub(crate) fn text_char_for_key(key: &TerminalKey) -> Option<char> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let KeyCode::Char(ch) = key.code else {
        return None;
    };

    if key.modifiers.is_empty() {
        return Some(ch);
    }
    if key.modifiers == KeyModifiers::SHIFT {
        return shifted_text_char(key, ch);
    }
    None
}

fn shifted_text_char(key: &TerminalKey, ch: char) -> Option<char> {
    if let Some(shifted) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(shifted);
    }

    if ch.is_ascii_uppercase() {
        return Some(ch);
    }

    if ch.is_ascii_lowercase() {
        return Some(ch.to_ascii_uppercase());
    }

    if is_shifted_ascii_punctuation(ch) {
        return Some(ch);
    }

    None
}

fn is_shifted_ascii_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '!' | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '{'
            | '}'
            | '|'
            | ':'
            | '"'
            | '<'
            | '>'
            | '?'
            | '~'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_types_its_character_and_a_shifted_press_its_shifted_form() {
        let plain = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(text_char_for_key(&plain), Some('a'));
        let shifted = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::SHIFT);
        assert_eq!(text_char_for_key(&shifted), Some('A'));
        let punctuation = TerminalKey::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert_eq!(text_char_for_key(&punctuation), Some('?'));
    }

    #[test]
    fn a_control_chord_or_a_non_character_key_types_nothing() {
        let chord = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(text_char_for_key(&chord), None);
        let enter = TerminalKey::new(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(text_char_for_key(&enter), None);
    }
}
