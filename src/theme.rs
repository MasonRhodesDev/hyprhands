//! How the overlay looks: colours, the frame's tape, the click ripple, the movement tail, the
//! caption, and whether it is drawn smooth or as chunky pixels.
//!
//! Built in: `construction` (the default): 8-bit hazard tape, hi-vis yellow on black, a lime
//! ripple and a safety-orange tail, barrier-tape red and white once stopped; and `adaptive`:
//! smooth, coloured as the complement of the owner's window border so it never blends into their
//! theme. An owner's own theme is a TOML file, `$XDG_CONFIG_HOME/hyprhands/themes/NAME.toml` or
//! any path, naming a built-in to start from and the fields it changes:
//!
//! ```toml
//! extends = "construction"
//! stripe  = ["#ff00aa", "#101010"]
//! pixel   = 2
//! ```
//!
//! Colours are `#rrggbb` or `#rrggbbaa`. `pixel` is the size of one drawn pixel in logical px:
//! 1 is smooth, 2 and up is 8-bit (fades become ordered dithering).

use crate::draw::Rgba;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;

/// Who has the seat, which the colours say.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Driving,
    /// The owner took over, or the panic file: hyprhands refuses input.
    Stopped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    /// Logical px per drawn pixel: 1 smooth, 2+ 8-bit.
    pub pixel: u32,
    /// The frame's two stripe colours while driving, and once stopped.
    pub stripe: [Rgba; 2],
    pub stopped: [Rgba; 2],
    /// A rim on both sides of the tape, or none.
    pub rim: Option<Rgba>,
    pub click: Rgba,
    pub trail: Rgba,
    /// The optional seat-wide wash (`serve --tint`).
    pub tint: Rgba,
    pub caption_bg: Rgba,
    pub caption_fg: Rgba,
    pub caption_border: Rgba,
    /// Caption text size in logical px (an 8-bit theme needs it larger to stay legible).
    pub caption_px: f32,
}

impl Theme {
    pub fn frame(&self, m: Mode) -> [Rgba; 2] {
        match m {
            Mode::Driving => self.stripe,
            Mode::Stopped => self.stopped,
        }
    }

    /// The default: 8-bit construction site.
    pub fn construction() -> Self {
        let black = hex("#141414");
        Self {
            name: "construction".into(),
            pixel: 3,
            stripe: [hex("#ffc400"), black],
            stopped: [hex("#e5202a"), hex("#f4f4f4")],
            rim: None,
            click: hex("#b6ff00"),
            trail: hex("#ff6a13"),
            tint: hex("#ffc400"),
            caption_bg: Rgba(0x14, 0x14, 0x14, 0xf0),
            caption_fg: hex("#ffc400"),
            caption_border: hex("#ffc400"),
            caption_px: 30.0,
        }
    }

    /// Smooth, and turned around from the owner's accent (their active border colour): the
    /// complement of its hue at full saturation for the frame and the click, the hue a third of
    /// the way round for the movement tail. A grey accent (no hue) falls back to orange.
    pub fn adaptive(accent: Option<Rgba>) -> Self {
        let driving_hue = accent
            .and_then(|Rgba(r, g, b, _)| {
                let (h, s, _) = hsl(r, g, b);
                (s > 0.12).then_some((h + 180.0) % 360.0)
            })
            // Too near the stopped red reads as stopped: pushed to orange.
            .map_or(30.0, |h| if (25.0..=330.0).contains(&h) { h } else { 30.0 });
        let driving = from_hsl(driving_hue, 1.0, 0.52);
        let stopped = hex("#ff3b4a");
        let shade = |c: Rgba| Rgba((u32::from(c.0) * 2 / 5) as u8, (u32::from(c.1) * 2 / 5) as u8, (u32::from(c.2) * 2 / 5) as u8, 0xff);
        Self {
            name: "adaptive".into(),
            pixel: 1,
            stripe: [driving, shade(driving)],
            stopped: [stopped, shade(stopped)],
            rim: Some(Rgba(0, 0, 0, 0xc0)),
            click: driving,
            trail: from_hsl((driving_hue + 120.0) % 360.0, 0.9, 0.55),
            tint: driving,
            caption_bg: Rgba(0x2a, 0x2c, 0x34, 0xf0),
            caption_fg: Rgba(0xf2, 0xf2, 0xf2, 0xff),
            caption_border: driving,
            caption_px: 18.0,
        }
    }

    fn builtin(name: &str, accent: Option<Rgba>) -> Option<Self> {
        match name {
            "construction" => Some(Self::construction()),
            "adaptive" => Some(Self::adaptive(accent)),
            _ => None,
        }
    }

    /// A built-in by name, a theme file by path, or `NAME.toml` in the owner's themes directory.
    pub fn load(spec: &str, accent: Option<Rgba>) -> Result<Self> {
        if let Some(t) = Self::builtin(spec, accent) {
            return Ok(t);
        }
        let path = if spec.ends_with(".toml") || spec.contains('/') { PathBuf::from(spec) } else { themes_dir().join(format!("{spec}.toml")) };
        let text = std::fs::read_to_string(&path).with_context(|| format!("theme {spec:?}: reading {}", path.display()))?;
        let file: ThemeFile = toml::from_str(&text).with_context(|| format!("theme {}", path.display()))?;
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or(spec).to_owned();
        file.apply(name, accent)
    }
}

fn themes_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("hyprhands/themes")
}

/// A theme file: a built-in to start from and the fields it changes.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    extends: Option<String>,
    pixel: Option<u32>,
    stripe: Option<[String; 2]>,
    stopped: Option<[String; 2]>,
    rim: Option<String>,
    click: Option<String>,
    trail: Option<String>,
    tint: Option<String>,
    caption_bg: Option<String>,
    caption_fg: Option<String>,
    caption_border: Option<String>,
    caption_px: Option<f32>,
}

impl ThemeFile {
    fn apply(self, name: String, accent: Option<Rgba>) -> Result<Theme> {
        let base = self.extends.as_deref().unwrap_or("construction");
        let Some(mut t) = Theme::builtin(base, accent) else { bail!("extends {base:?}: not a built-in theme (construction, adaptive)") };
        t.name = name;
        let c = |s: &str| parse_hex(s).with_context(|| format!("colour {s:?}"));
        if let Some(p) = self.pixel {
            t.pixel = p.clamp(1, 8);
        }
        if let Some([a, b]) = self.stripe {
            t.stripe = [c(&a)?, c(&b)?];
        }
        if let Some([a, b]) = self.stopped {
            t.stopped = [c(&a)?, c(&b)?];
        }
        if let Some(r) = self.rim {
            t.rim = if r == "none" { None } else { Some(c(&r)?) };
        }
        for (field, v) in [
            (&mut t.click, self.click),
            (&mut t.trail, self.trail),
            (&mut t.tint, self.tint),
            (&mut t.caption_bg, self.caption_bg),
            (&mut t.caption_fg, self.caption_fg),
            (&mut t.caption_border, self.caption_border),
        ] {
            if let Some(v) = v {
                *field = c(&v)?;
            }
        }
        if let Some(px) = self.caption_px {
            t.caption_px = px.clamp(8.0, 96.0);
        }
        Ok(t)
    }
}

/// `#rrggbb` or `#rrggbbaa`.
pub fn parse_hex(s: &str) -> Result<Rgba> {
    let h = s.trim().trim_start_matches('#');
    let n = u32::from_str_radix(h, 16).context("not hex")?;
    match h.len() {
        6 => Ok(Rgba((n >> 16) as u8, (n >> 8) as u8, n as u8, 0xff)),
        8 => Ok(Rgba((n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8)),
        _ => bail!("want #rrggbb or #rrggbbaa"),
    }
}

fn hex(s: &str) -> Rgba {
    parse_hex(s).expect("a built-in colour")
}

pub fn hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

pub fn from_hsl(h: f32, s: f32, l: f32) -> Rgba {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgba(to(r), to(g), to(b), 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_turns_the_owners_theme_around() {
        let t = Theme::adaptive(Some(Rgba(0xa4, 0xc9, 0xfe, 0xff)));
        let (h, s, _) = hsl(t.stripe[0].0, t.stripe[0].1, t.stripe[0].2);
        assert!((30.0..=40.0).contains(&h) && s > 0.95, "hue {h} sat {s}");
        let (th, _, _) = hsl(t.trail.0, t.trail.1, t.trail.2);
        assert!((th - h - 120.0).abs() < 3.0, "the tail is a third of the way round: {th}");
        assert_eq!(Theme::adaptive(None).stripe, Theme::adaptive(Some(Rgba(0x8d, 0x91, 0x99, 0xff))).stripe, "grey: fallback");
        let teal = Theme::adaptive(Some(Rgba(0x00, 0xc0, 0xc0, 0xff)));
        assert!(hsl(teal.stripe[0].0, teal.stripe[0].1, teal.stripe[0].2).0 >= 25.0, "never the stopped red");
    }

    #[test]
    fn construction_is_the_8_bit_default_with_click_and_tail_apart() {
        let t = Theme::load("construction", None).unwrap();
        assert!(t.pixel > 1);
        assert_ne!(t.click, t.trail);
        assert_ne!(t.stripe, t.stopped);
    }

    #[test]
    fn a_theme_file_extends_a_built_in() {
        let dir = std::env::temp_dir().join(format!("hyprhands-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("neon.toml");
        std::fs::write(&f, "extends = \"adaptive\"\nstripe = [\"#ff00aa\", \"#101010\"]\npixel = 2\nrim = \"none\"\nclick = \"#00ffffcc\"\n").unwrap();
        let t = Theme::load(f.to_str().unwrap(), None).unwrap();
        assert_eq!((t.name.as_str(), t.pixel, t.rim), ("neon", 2, None));
        assert_eq!(t.stripe[0], Rgba(0xff, 0x00, 0xaa, 0xff));
        assert_eq!(t.click, Rgba(0x00, 0xff, 0xff, 0xcc));
        std::fs::write(&f, "colour = \"#fff\"\n").unwrap();
        assert!(Theme::load(f.to_str().unwrap(), None).is_err(), "unknown fields are refused");
        assert!(Theme::load("no-such-theme-anywhere", None).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
