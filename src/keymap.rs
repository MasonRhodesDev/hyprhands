//! XKB keymaps generated for exactly the keys an input needs, so typing is layout-independent
//! and unicode-correct: the Nth key of the keymap produces the Nth character.
//!
//! Approach from hypruse's `wire.py` (MIT, Ilyas Khallouki), itself after wtype: characters as
//! `U<hex>` keysyms, and a combo's modifiers as real keys with a `modifier_map`, pressed and
//! released like a physical keyboard's, so no modifier-mask numbering has to be agreed on.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::fmt::Write;

/// xkb keycode = evdev code + 8.
pub const KEYCODE_BASE: u32 = 8;

/// A keymap and the evdev code of each entry, by label.
pub struct Keymap {
    pub text: String,
    pub codes: HashMap<String, u32>,
}

fn build(entries: &[(String, String)], modmap: &[(String, String)]) -> Keymap {
    let mut kc = String::new();
    let mut sy = String::new();
    let mut codes = HashMap::new();
    for (i, (label, keysym)) in entries.iter().enumerate() {
        let code = KEYCODE_BASE + 1 + i as u32;
        let _ = writeln!(kc, "    <{label}> = {code};");
        let _ = writeln!(sy, "    key <{label}> {{ [ {keysym} ] }};");
        codes.insert(label.clone(), code - KEYCODE_BASE);
    }
    for (xkb_mod, label) in modmap {
        let _ = writeln!(sy, "    modifier_map {xkb_mod} {{ <{label}> }};");
    }
    let text = format!(
        "xkb_keymap {{\n  xkb_keycodes {{\n    minimum = {min};\n    maximum = {max};\n{kc}  }};\n  \
         xkb_types {{ include \"complete\" }};\n  xkb_compat {{ include \"complete\" }};\n  \
         xkb_symbols \"(unnamed)\" {{\n{sy}  }};\n}};\n",
        min = KEYCODE_BASE,
        max = KEYCODE_BASE + entries.len() as u32 + 1,
    );
    Keymap { text, codes }
}

pub fn unicode_keysym(ch: char) -> String {
    format!("U{:04X}", ch as u32)
}

/// One key per distinct character, labelled `C<index>`; returns the keymap and the characters in
/// key order.
pub fn for_text(text: &str) -> (Keymap, Vec<char>) {
    let mut chars: Vec<char> = Vec::new();
    for ch in text.chars() {
        if !chars.contains(&ch) {
            chars.push(ch);
        }
    }
    let entries: Vec<(String, String)> = chars.iter().enumerate().map(|(i, c)| (format!("C{i}"), unicode_keysym(*c))).collect();
    (build(&entries, &[]), chars)
}

/// A parsed combo like `ctrl+shift+t`, `esc`, `F5`, `alt+Left`.
#[derive(Debug, PartialEq)]
pub struct Combo {
    pub mods: Vec<&'static str>,
    /// The key as named in the combo (`t`, `Left`, `F5`), for matching the owner's binds.
    pub key: String,
    pub keysym: String,
}

const MODS: [(&str, &str, &str); 5] = [
    // name, keysym, xkb real modifier
    ("shift", "Shift_L", "Shift"),
    ("ctrl", "Control_L", "Control"),
    ("alt", "Alt_L", "Mod1"),
    ("super", "Super_L", "Mod4"),
    ("altgr", "ISO_Level3_Shift", "Mod5"),
];

fn named_key(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "tab" => "Tab",
        "space" => "space",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" | "prior" => "Prior",
        "pagedown" | "page_down" | "next" => "Next",
        "insert" => "Insert",
        _ => return None,
    })
}

pub fn parse_combo(combo: &str) -> Result<Combo> {
    let parts: Vec<&str> = combo.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let Some((key, mod_names)) = parts.split_last() else { bail!("empty key combo") };
    let mut mods = Vec::new();
    for m in mod_names {
        let lower = m.to_ascii_lowercase();
        let lower = match lower.as_str() {
            "control" => "ctrl",
            "logo" | "win" | "meta" => "super",
            other => other,
        };
        match MODS.iter().find(|(name, _, _)| *name == lower) {
            Some((name, _, _)) => mods.push(*name),
            None => bail!("unknown modifier {m:?} in {combo:?}"),
        }
    }
    let keysym = if let Some(k) = named_key(key) {
        k.to_owned()
    } else if key.len() > 1 && key[1..].chars().all(|c| c.is_ascii_digit()) && key.starts_with(['F', 'f']) {
        format!("F{}", &key[1..])
    } else if key.chars().count() == 1 {
        // a letter combined with ctrl/alt means the lowercase key, as a physical keyboard sends
        let c = key.chars().next().unwrap();
        unicode_keysym(if mods.is_empty() { c } else { c.to_ascii_lowercase() })
    } else {
        bail!("unknown key {key:?} in {combo:?}");
    };
    Ok(Combo { mods, key: named_key(key).map_or_else(|| (*key).to_owned(), str::to_owned), keysym })
}

/// The real-modifier bit for a combo modifier name, in xkb's fixed order (Shift, Lock, Control,
/// Mod1..Mod5): what `zwp_virtual_keyboard_v1.modifiers` takes as its depressed mask.
pub fn mod_mask(name: &str) -> u32 {
    let real = MODS.iter().find(|(n, _, _)| *n == name).map_or("", |m| m.2);
    ["Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5"].iter().position(|r| *r == real).map_or(0, |i| 1 << i)
}

/// The keymap for one combo: its modifier keys (`M_<name>`) plus the key (`KEY`).
pub fn for_combo(combo: &Combo) -> Keymap {
    let mut entries = Vec::new();
    let mut modmap = Vec::new();
    for m in &combo.mods {
        let (_, keysym, xkb_mod) = MODS.iter().find(|(n, _, _)| n == m).unwrap();
        entries.push((format!("M_{m}"), (*keysym).to_owned()));
        modmap.push(((*xkb_mod).to_owned(), format!("M_{m}")));
    }
    entries.push(("KEY".to_owned(), combo.keysym.clone()));
    build(&entries, &modmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_gets_one_key_per_distinct_character_in_order() {
        let (km, chars) = for_text("héllo €");
        assert_eq!(chars, vec!['h', 'é', 'l', 'o', ' ', '€']);
        assert!(km.text.contains("key <C1> { [ U00E9 ] };"));
        assert!(km.text.contains("key <C5> { [ U20AC ] };"));
        assert_eq!(km.codes["C0"], 1);
        assert!(km.text.contains("maximum = 15;"));
    }

    #[test]
    fn combos_parse_to_modifier_keys_and_a_keysym() {
        assert_eq!(
            parse_combo("ctrl+shift+T").unwrap(),
            Combo { mods: vec!["ctrl", "shift"], key: "T".into(), keysym: "U0074".into() }
        );
        assert_eq!(parse_combo("esc").unwrap().keysym, "Escape");
        assert_eq!(parse_combo("alt+Left").unwrap(), Combo { mods: vec!["alt"], key: "Left".into(), keysym: "Left".into() });
        assert_eq!(parse_combo("super+enter").unwrap().key, "Return", "matched against binds by keysym name");
        assert_eq!(parse_combo("F5").unwrap().keysym, "F5");
        assert_eq!(parse_combo("A").unwrap().keysym, "U0041", "a bare capital types a capital");
        assert!(parse_combo("hyper+x").is_err());
        assert!(parse_combo("").is_err());
    }

    #[test]
    fn a_combo_keymap_maps_its_modifiers() {
        let km = for_combo(&parse_combo("ctrl+a").unwrap());
        assert!(km.text.contains("key <M_ctrl> { [ Control_L ] };"));
        assert!(km.text.contains("modifier_map Control { <M_ctrl> };"));
        assert_eq!((km.codes["M_ctrl"], km.codes["KEY"]), (1, 2));
        assert_eq!([mod_mask("shift"), mod_mask("ctrl"), mod_mask("alt"), mod_mask("super"), mod_mask("altgr")], [1, 4, 8, 64, 128]);
    }
}
