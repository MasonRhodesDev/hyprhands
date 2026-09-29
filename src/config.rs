//! The owner's LOADED Hyprland config, read over IPC and never written.
//!
//! hyprhands does not edit config files and does not add runtime binds or keywords: everything
//! here comes from queries (`binds`, `getoption`, `devices`). What it is for:
//!
//! - telling an agent what the owner's keys do (SUPER+Q opens a terminal, and so on), so it can
//!   drive the owner's own launchers instead of guessing;
//! - refusing to send a combo the compositor would catch as a bind, unless the caller says it
//!   means to trigger that bind: app-level keys must reach the app;
//! - knowing the input behaviour that changes what an action does (focus follows the mouse,
//!   the owner's layout and repeat settings).

use crate::hypr::Instance;
use crate::keymap::Combo;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

/// Hyprland's modifier mask bits, in its own names.
const MASK: [(u32, &str); 8] = [
    (1, "shift"),
    (2, "caps"),
    (4, "ctrl"),
    (8, "alt"),
    (16, "mod2"),
    (32, "mod3"),
    (64, "super"),
    (128, "mod5"),
];
/// Lock-style modifiers that do not change which bind a key hits.
const LOCKS: [&str; 2] = ["caps", "mod2"];

/// Options that change what an input does.
pub const OPTIONS: [&str; 7] = [
    "input:follow_mouse",
    "input:kb_layout",
    "input:kb_variant",
    "input:kb_options",
    "input:repeat_rate",
    "input:repeat_delay",
    "cursor:no_warps",
];

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Bind {
    pub mods: Vec<&'static str>,
    pub key: String,
    pub description: String,
    pub dispatcher: String,
    pub arg: String,
    pub submap: String,
    pub mouse: bool,
    pub release: bool,
    pub non_consuming: bool,
    pub locked: bool,
}

impl Bind {
    pub fn from_json(b: &Value) -> Self {
        let mask = b["modmask"].as_u64().unwrap_or(0) as u32;
        Self {
            mods: MASK.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, n)| *n).collect(),
            key: b["key"].as_str().unwrap_or_default().to_owned(),
            description: b["description"].as_str().unwrap_or_default().to_owned(),
            dispatcher: b["dispatcher"].as_str().unwrap_or_default().to_owned(),
            arg: b["arg"].as_str().unwrap_or_default().to_owned(),
            submap: b["submap"].as_str().unwrap_or_default().to_owned(),
            mouse: b["mouse"].as_bool().unwrap_or(false),
            release: b["release"].as_bool().unwrap_or(false),
            non_consuming: b["non_consuming"].as_bool().unwrap_or(false),
            locked: b["locked"].as_bool().unwrap_or(false),
        }
    }

    /// Whether pressing `combo` in the default submap would fire this bind (and, unless the bind
    /// is non-consuming, never reach the focused app).
    pub fn catches(&self, combo: &Combo) -> bool {
        if self.mouse || !self.submap.is_empty() {
            return false;
        }
        let mut ours: Vec<&str> = self.mods.iter().copied().filter(|m| !LOCKS.contains(m)).collect();
        let mut theirs: Vec<&str> = combo.mods.clone();
        ours.sort_unstable();
        theirs.sort_unstable();
        ours == theirs && self.key.eq_ignore_ascii_case(&combo.key)
    }

    pub fn label(&self) -> String {
        let mut parts: Vec<String> = self.mods.iter().map(|m| m.to_uppercase()).collect();
        parts.push(self.key.clone());
        parts.join("+")
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct Config {
    pub binds: Vec<Bind>,
    pub options: serde_json::Map<String, Value>,
    pub keyboards: Vec<Value>,
}

impl Config {
    pub fn load(instance: &Instance) -> Result<Self> {
        let binds = instance.query("binds")?.as_array().map(|a| a.iter().map(Bind::from_json).collect()).unwrap_or_default();
        let mut options = serde_json::Map::new();
        for name in OPTIONS {
            if let Ok(v) = instance.query(&format!("getoption {name}")) {
                options.insert(name.to_owned(), v);
            }
        }
        let keyboards = instance.query("devices").ok().and_then(|d| d["keyboards"].as_array().cloned()).unwrap_or_default();
        Ok(Self { binds, options, keyboards })
    }

    /// The bind that would swallow `combo`, if any. Non-consuming binds still pass the key on, so
    /// they do not count.
    pub fn swallowed_by(&self, combo: &Combo) -> Option<&Bind> {
        self.binds.iter().find(|b| !b.non_consuming && b.catches(combo))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::parse_combo;
    use serde_json::json;

    fn cfg() -> Config {
        Config {
            binds: [
                json!({"modmask": 64, "key": "Q", "description": "Apps: Terminal", "dispatcher": "exec_cmd", "submap": ""}),
                json!({"modmask": 65, "key": "BackSpace", "dispatcher": "exec_cmd", "submap": ""}),
                json!({"modmask": 0, "key": "Escape", "dispatcher": "submap", "submap": "resize"}),
                json!({"modmask": 4, "key": "F13", "non_consuming": true, "submap": ""}),
            ]
            .iter()
            .map(Bind::from_json)
            .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn modmask_decodes_to_names_and_labels() {
        let c = cfg();
        assert_eq!(c.binds[1].mods, vec!["shift", "super"]);
        assert_eq!(c.binds[1].label(), "SHIFT+SUPER+BackSpace");
    }

    #[test]
    fn a_combo_a_bind_would_swallow_is_found_case_insensitively() {
        let c = cfg();
        assert_eq!(c.swallowed_by(&parse_combo("super+q").unwrap()).unwrap().description, "Apps: Terminal");
        assert!(c.swallowed_by(&parse_combo("super+shift+backspace").unwrap()).is_some());
        assert!(c.swallowed_by(&parse_combo("ctrl+q").unwrap()).is_none());
    }

    #[test]
    fn submap_mouse_and_non_consuming_binds_let_the_key_through() {
        let c = cfg();
        assert!(c.swallowed_by(&parse_combo("esc").unwrap()).is_none(), "only active inside its submap");
        assert!(c.swallowed_by(&parse_combo("ctrl+F13").unwrap()).is_none(), "non-consuming");
    }

    #[test]
    fn a_numlock_bit_does_not_hide_a_bind() {
        let b = Bind::from_json(&json!({"modmask": 64 | 16, "key": "Return", "submap": ""}));
        assert!(b.catches(&parse_combo("super+Return").unwrap()));
    }
}
