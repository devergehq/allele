//! Terminal keymap — policy-based key-to-bytes translation.
//!
//! Instead of hardcoding every modifier+key combination as individual match
//! arms, this module separates concerns into:
//!
//! 1. **App actions** — Cmd-key shortcuts handled by Allele (copy, paste,
//!    zoom, session management). These never reach the PTY.
//! 2. **Terminal input** — bytes sent to the PTY, governed by:
//!    - A **base sequence table** mapping unmodified special keys to byte
//!      sequences (enter → `\r`, up → `\x1b[A`, etc.).
//!    - **Modifier policies** applied generically (e.g. "Option key sends
//!      ESC prefix") rather than per-key match arms.
//!    - A small **readline override table** for Option+key combos that need
//!      readline-friendly sequences instead of raw xterm encoding.

use gpui::Modifiers;

// ---------------------------------------------------------------------------
// App actions — Allele-level shortcuts that don't reach the PTY
// ---------------------------------------------------------------------------

/// Actions handled by the terminal view itself (not sent to the PTY).
#[derive(Debug, Clone, PartialEq)]
pub enum AppAction {
    Paste,
    Copy,
    OpenSearch,
    FindNext,
    FindPrevious,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    NewSession,
    CloseSession,
    PrevSession,
    NextSession,
    SwitchSession(usize),
    ToggleDrawer,
    ToggleSidebar,
    ToggleRightSidebar,
    /// Open the scratch pad compose overlay (Cmd+K).
    OpenScratchPad,
    /// Send raw bytes to the PTY (for Cmd shortcuts that map to control
    /// characters, e.g. Cmd+Backspace → 0x15).
    SendBytes(&'static [u8]),
}

/// Look up an app-level action for a platform-modifier keystroke.
///
/// Returns `None` if the keystroke isn't an app shortcut — it should be
/// forwarded to the terminal input path instead.
pub fn app_action(key: &str, mods: &Modifiers) -> Option<AppAction> {
    if !mods.platform {
        return None;
    }

    Some(match key {
        "v" => AppAction::Paste,
        "c" => AppAction::Copy,
        "f" => AppAction::OpenSearch,
        "g" if mods.shift => AppAction::FindPrevious,
        "g" => AppAction::FindNext,
        "=" | "+" => AppAction::ZoomIn,
        "-" | "_" => AppAction::ZoomOut,
        "0" => AppAction::ZoomReset,
        "n" => AppAction::NewSession,
        "w" => AppAction::CloseSession,
        "[" => AppAction::PrevSession,
        "]" => AppAction::NextSession,
        "j" => AppAction::ToggleDrawer,
        "k" => AppAction::OpenScratchPad,
        "b" if mods.alt => AppAction::ToggleRightSidebar,
        "b" => AppAction::ToggleSidebar,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
            let idx = key.parse::<usize>().unwrap_or(1) - 1;
            AppAction::SwitchSession(idx)
        }
        // Cmd+Left/Right → readline beginning/end-of-line
        "left" => AppAction::SendBytes(b"\x01"),
        "right" => AppAction::SendBytes(b"\x05"),
        // Cmd+Backspace → readline backward-kill-line
        "backspace" => AppAction::SendBytes(b"\x15"),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Terminal input — modifier policies + sequence tables
// ---------------------------------------------------------------------------

/// How the Option/Alt key modifies terminal input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Normal variant used when loaded from settings
pub enum OptionKeyBehaviour {
    /// Option key has no special terminal effect (macOS default in
    /// Terminal.app). GPUI may still provide a `key_char` with the
    /// Option-modified character (e.g. Option+P → π).
    Normal,
    /// Option key acts as Meta — prefixes the base sequence with ESC
    /// (0x1b). This is what power users expect and what iTerm2 calls
    /// "Esc+" mode.
    Meta,
}

/// Configuration for how keystrokes are translated to PTY bytes.
///
/// Sane defaults are baked in via `Default`. Users can override via
/// `settings.json` in the future.
#[derive(Debug, Clone)]
pub struct KeymapConfig {
    pub option_key: OptionKeyBehaviour,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            option_key: OptionKeyBehaviour::Meta,
        }
    }
}

/// Base byte sequences for special (non-printable) keys, unmodified.
fn base_sequence(key: &str) -> Option<&'static [u8]> {
    Some(match key {
        "enter" => b"\r",
        "backspace" => b"\x7f",
        "tab" => b"\t",
        "escape" => b"\x1b",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "delete" => b"\x1b[3~",
        "space" => b" ",
        _ => return None,
    })
}

/// Readline-friendly overrides for Option+key combos.
///
/// Some Option+key combinations have readline-specific sequences that
/// differ from what raw "ESC + base_sequence" would produce. For example,
/// Option+Left should send `ESC b` (backward-word) not `ESC ESC[D`.
fn readline_alt_override(key: &str) -> Option<&'static [u8]> {
    Some(match key {
        "left" => b"\x1bb",        // backward-word
        "right" => b"\x1bf",       // forward-word
        "backspace" => b"\x1b\x7f", // backward-kill-word
        "delete" => b"\x1bd",       // forward-kill-word
        _ => return None,
    })
}

impl KeymapConfig {
    /// Resolve a keystroke into bytes to send to the PTY.
    ///
    /// Returns `None` if the keystroke doesn't map to any terminal input
    /// (e.g. an unrecognised key with no `key_char`). The caller should
    /// have already checked for app actions via [`app_action()`].
    ///
    /// `key_char` is the character representation from the OS (e.g. "a",
    /// "π"), if any.
    pub fn resolve(&self, key: &str, mods: &Modifiers, key_char: Option<&str>) -> Option<Vec<u8>> {
        // 0. Shift+Enter → literal newline (\n) instead of carriage return.
        //    Claude Code's input editor treats \n as "insert line break" and
        //    \r as "submit", matching standard macOS text-input behaviour.
        if mods.shift && key == "enter" {
            return Some(b"\n".to_vec());
        }

        // Shift+Tab → CSI Z (back-tab). xterm-standard sequence that TUIs
        // like Claude Code rely on for reverse-tab navigation.
        if mods.shift && key == "tab" {
            return Some(b"\x1b[Z".to_vec());
        }

        // 1. Control key — algorithmic: letter → control byte
        if mods.control {
            if let Some(byte) = control_byte(key) {
                return Some(vec![byte]);
            }
        }

        // 2. Alt/Option modifier — policy-based
        if mods.alt && self.option_key == OptionKeyBehaviour::Meta {
            // Check readline-specific overrides first
            if let Some(bytes) = readline_alt_override(key) {
                return Some(bytes.to_vec());
            }

            // For special keys, ESC-prefix the base sequence
            if let Some(base) = base_sequence(key) {
                let mut out = Vec::with_capacity(1 + base.len());
                out.push(0x1b);
                out.extend_from_slice(base);
                return Some(out);
            }

            // For printable characters, ESC-prefix the *base* key (not the
            // OS-composed key_char which may be a dead-key or accented char).
            // This matches how iTerm2's "Esc+" mode works.
            let key_bytes = key.as_bytes();
            if key_bytes.len() == 1 && key_bytes[0].is_ascii_graphic() {
                return Some(vec![0x1b, key_bytes[0]]);
            }
        }

        // 3. Special keys (unmodified, or with Alt in Normal mode)
        if let Some(bytes) = base_sequence(key) {
            return Some(bytes.to_vec());
        }

        // 4. Regular printable character
        if let Some(ch) = key_char {
            return Some(ch.as_bytes().to_vec());
        }

        None
    }
}

/// Convert a single lowercase letter to its ASCII control byte.
///
/// Control characters are `byte & 0x1f` — i.e. 'a' (0x61) → 0x01,
/// 'z' (0x7a) → 0x1a. This is algorithmic, not a lookup table.
fn control_byte(key: &str) -> Option<u8> {
    let bytes = key.as_bytes();
    if bytes.len() == 1 {
        let b = bytes[0];
        if b.is_ascii_lowercase() {
            return Some(b & 0x1f);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(control: bool, alt: bool, platform: bool, shift: bool) -> Modifiers {
        Modifiers { control, alt, platform, shift, function: false }
    }

    fn no_mods() -> Modifiers {
        mods(false, false, false, false)
    }

    #[test]
    fn plain_enter() {
        let km = KeymapConfig::default();
        assert_eq!(km.resolve("enter", &no_mods(), None), Some(b"\r".to_vec()));
    }

    #[test]
    fn option_enter_meta() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("enter", &mods(false, true, false, false), None),
            Some(b"\x1b\r".to_vec())
        );
    }

    #[test]
    fn option_left_readline() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("left", &mods(false, true, false, false), None),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn option_right_readline() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("right", &mods(false, true, false, false), None),
            Some(b"\x1bf".to_vec())
        );
    }

    #[test]
    fn option_backspace_readline() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("backspace", &mods(false, true, false, false), None),
            Some(b"\x1b\x7f".to_vec())
        );
    }

    #[test]
    fn option_delete_readline() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("delete", &mods(false, true, false, false), None),
            Some(b"\x1bd".to_vec())
        );
    }

    #[test]
    fn control_a() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("a", &mods(true, false, false, false), None),
            Some(vec![0x01])
        );
    }

    #[test]
    fn control_z() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("z", &mods(true, false, false, false), None),
            Some(vec![0x1a])
        );
    }

    #[test]
    fn control_c() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("c", &mods(true, false, false, false), None),
            Some(vec![0x03])
        );
    }

    #[test]
    fn regular_char() {
        let km = KeymapConfig::default();
        assert_eq!(
            km.resolve("a", &no_mods(), Some("a")),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn option_normal_mode_no_esc_prefix() {
        let km = KeymapConfig { option_key: OptionKeyBehaviour::Normal };
        // In Normal mode, Option+Left should just send plain left arrow
        assert_eq!(
            km.resolve("left", &mods(false, true, false, false), None),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn app_action_cmd_left() {
        let action = app_action("left", &mods(false, false, true, false));
        assert_eq!(action, Some(AppAction::SendBytes(b"\x01")));
    }

    #[test]
    fn app_action_cmd_right() {
        let action = app_action("right", &mods(false, false, true, false));
        assert_eq!(action, Some(AppAction::SendBytes(b"\x05")));
    }

    #[test]
    fn app_action_cmd_backspace() {
        let action = app_action("backspace", &mods(false, false, true, false));
        assert_eq!(action, Some(AppAction::SendBytes(b"\x15")));
    }

    #[test]
    fn app_action_returns_none_without_platform() {
        assert_eq!(app_action("v", &no_mods()), None);
    }

    #[test]
    fn option_printable_char_meta_mode() {
        let km = KeymapConfig::default();
        // Option+a should send ESC + 'a' (base key, not OS-composed 'å')
        assert_eq!(
            km.resolve("a", &mods(false, true, false, false), Some("å")),
            Some(vec![0x1b, b'a'])
        );
    }
}
