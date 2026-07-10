//! Keymap tables — PROTOCOL.md §3.5 / VECTORS.md §3.
//!
//! Input `key` events carry **evdev** keycodes on the wire. The Quest client
//! translates Android `AKEYCODE_*` → evdev; the macOS host translates
//! evdev → `CGKeyCode`. Both tables live as CSV data files in the spec repo
//! (`keymaps/*.csv`) so the two independent implementations cannot drift.
//!
//! This module only *parses* CSV text (no I/O — the caller reads the file);
//! that keeps `loom-proto` free of filesystem access per the crate contract.

use std::collections::HashMap;

use crate::error::{Error, Result};

/// A one-directional keycode lookup table parsed from a `keymaps/*.csv` file.
///
/// The CSV format is `# comment` lines and blank lines ignored, otherwise
/// `from,to` integer pairs, one per line.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    map: HashMap<i64, i64>,
}

impl Keymap {
    /// Parse a keymap from CSV text. Unmapped codes simply won't be present.
    pub fn from_csv(text: &str) -> Result<Self> {
        let mut map = HashMap::new();
        for (idx, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split(',');
            let from = parts.next();
            let to = parts.next();
            let (from, to) = match (from, to) {
                (Some(a), Some(b)) => (a.trim(), b.trim()),
                _ => {
                    return Err(Error::Keymap(format!(
                        "line {}: expected 'from,to'",
                        idx + 1
                    )))
                }
            };
            let from: i64 = from
                .parse()
                .map_err(|_| Error::Keymap(format!("line {}: bad 'from' value {from:?}", idx + 1)))?;
            let to: i64 = to
                .parse()
                .map_err(|_| Error::Keymap(format!("line {}: bad 'to' value {to:?}", idx + 1)))?;
            map.insert(from, to);
        }
        Ok(Self { map })
    }

    /// Look up a code. Returns `None` for unmapped codes — which the caller MUST
    /// swallow (§3.5: never send/inject an unmapped key).
    pub fn get(&self, code: i64) -> Option<i64> {
        self.map.get(&code).copied()
    }

    /// Number of entries (for sanity checks/tests).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_ignores_comments_and_blanks() {
        let csv = "# header comment\n\n7,11\n29,30\n  66 , 28 \n";
        let km = Keymap::from_csv(csv).unwrap();
        assert_eq!(km.len(), 3);
        assert_eq!(km.get(7), Some(11));
        assert_eq!(km.get(29), Some(30));
        assert_eq!(km.get(66), Some(28)); // whitespace around fields tolerated
    }

    #[test]
    fn unmapped_is_none() {
        let km = Keymap::from_csv("29,30\n").unwrap();
        assert_eq!(km.get(999), None);
    }

    #[test]
    fn rejects_malformed_rows() {
        assert!(Keymap::from_csv("29\n").is_err());
        assert!(Keymap::from_csv("29,notanumber\n").is_err());
        assert!(Keymap::from_csv("x,30\n").is_err());
    }
}
