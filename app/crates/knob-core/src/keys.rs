//! Keyboard shortcuts written as text, e.g. "cmd+shift+t" or "ctrl+k, ctrl+c".
//! Parsed here so both platforms read the same syntax; each platform maps
//! [`Key`] to its own key codes.

use std::fmt;

/// Modifier bits of a [`KeyCombo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    /// Command on macOS, Win on Windows.
    pub const META: Mods = Mods(1);
    pub const CTRL: Mods = Mods(2);
    /// Option on macOS.
    pub const ALT: Mods = Mods(4);
    pub const SHIFT: Mods = Mods(8);
    /// Command on macOS, Ctrl elsewhere ("mod+c" copies on both).
    pub const PRIMARY: Mods = Mods(16);

    pub fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Mods {
    type Output = Mods;
    fn bitor(self, rhs: Mods) -> Mods {
        Mods(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Mods {
    fn bitor_assign(&mut self, rhs: Mods) {
        self.0 |= rhs.0;
    }
}

/// A non-character key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Enter,
    Tab,
    Space,
    Escape,
    Backspace,
    /// Forward delete.
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// F1..=F20.
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A letter (always lowercase), digit or US-layout punctuation key:
    /// ` - = [ ] \ ; ' , . /
    Char(char),
    Named(NamedKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub mods: Mods,
    pub key: Key,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

const PUNCTUATION: &str = "`-=[]\\;',./";

fn parse_mod(word: &str) -> Option<Mods> {
    Some(match word {
        "cmd" | "command" | "meta" | "super" | "win" | "windows" => Mods::META,
        "ctrl" | "control" => Mods::CTRL,
        "alt" | "opt" | "option" => Mods::ALT,
        "shift" => Mods::SHIFT,
        "mod" | "cmdorctrl" | "primary" => Mods::PRIMARY,
        _ => return None,
    })
}

fn parse_key(word: &str) -> Option<Key> {
    let named = match word {
        "enter" | "return" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "space" => NamedKey::Space,
        "esc" | "escape" => NamedKey::Escape,
        "backspace" => NamedKey::Backspace,
        "delete" | "del" | "forwarddelete" => NamedKey::Delete,
        "up" => NamedKey::Up,
        "down" => NamedKey::Down,
        "left" => NamedKey::Left,
        "right" => NamedKey::Right,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" | "pgup" => NamedKey::PageUp,
        "pagedown" | "pgdn" => NamedKey::PageDown,
        "plus" => return Some(Key::Char('=')),
        "comma" => return Some(Key::Char(',')),
        "minus" => return Some(Key::Char('-')),
        _ => {
            if let Some(n) = word.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                if (1..=20).contains(&n) {
                    return Some(Key::Named(NamedKey::F(n)));
                }
            }
            let mut chars = word.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return (c.is_ascii_alphanumeric() || PUNCTUATION.contains(c)).then(|| Key::Char(c.to_ascii_lowercase()));
        }
    };
    Some(Key::Named(named))
}

impl KeyCombo {
    /// One combo like "cmd+shift+t". Case and spaces around '+' don't matter.
    pub fn parse(text: &str) -> Result<KeyCombo, ParseError> {
        let text = text.trim().to_ascii_lowercase();
        if text.is_empty() {
            return Err(ParseError("empty shortcut".into()));
        }
        // "cmd++" means Cmd and the '=' / '+' key
        let (body, plus_key) = match text.strip_suffix("++") {
            Some(rest) => (rest.to_string(), true),
            None => (text.clone(), false),
        };
        let mut parts: Vec<&str> = body.split('+').map(str::trim).collect();
        let key_word = if plus_key { "plus" } else { parts.pop().unwrap_or("") };
        let mut mods = Mods::NONE;
        for p in parts {
            mods |= parse_mod(p).ok_or_else(|| ParseError(format!("unknown modifier \"{p}\" in \"{text}\"")))?;
        }
        let key = parse_key(key_word).ok_or_else(|| ParseError(format!("unknown key \"{key_word}\" in \"{text}\"")))?;
        Ok(KeyCombo { mods, key })
    }

    /// A comma-separated sequence: "ctrl+k, ctrl+c". Use "comma" for the ',' key.
    pub fn parse_sequence(text: &str) -> Result<Vec<KeyCombo>, ParseError> {
        let combos = text.split(',').filter(|p| !p.trim().is_empty()).map(KeyCombo::parse).collect::<Result<Vec<_>, _>>()?;
        if combos.is_empty() {
            return Err(ParseError("empty shortcut".into()));
        }
        Ok(combos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_keys() {
        let c = KeyCombo::parse("Cmd + Shift + T").unwrap();
        assert_eq!(c, KeyCombo { mods: Mods::META | Mods::SHIFT, key: Key::Char('t') });
        assert_eq!(KeyCombo::parse("f5").unwrap().key, Key::Named(NamedKey::F(5)));
        assert_eq!(KeyCombo::parse("mod+[").unwrap(), KeyCombo { mods: Mods::PRIMARY, key: Key::Char('[') });
        assert_eq!(KeyCombo::parse("cmd++").unwrap(), KeyCombo { mods: Mods::META, key: Key::Char('=') });
        assert_eq!(KeyCombo::parse("ctrl+alt+delete").unwrap().key, Key::Named(NamedKey::Delete));
        assert_eq!(KeyCombo::parse("space").unwrap(), KeyCombo { mods: Mods::NONE, key: Key::Named(NamedKey::Space) });
    }

    #[test]
    fn parses_sequences() {
        let s = KeyCombo::parse_sequence("ctrl+k, ctrl+c").unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[1], KeyCombo { mods: Mods::CTRL, key: Key::Char('c') });
        assert_eq!(KeyCombo::parse_sequence("cmd+comma").unwrap()[0].key, Key::Char(','));
    }

    #[test]
    fn rejects_garbage() {
        assert!(KeyCombo::parse("hyper+x").is_err());
        assert!(KeyCombo::parse("cmd+").is_err());
        assert!(KeyCombo::parse("cmd+ab").is_err());
        assert!(KeyCombo::parse("f21").is_err());
        assert!(KeyCombo::parse_sequence(" , ").is_err());
    }
}
