//! The robot cursor: while a session drives, the pointer itself says so.
//!
//! A pointer drawn by an overlay has to chase the real one (it is polled, then moved), so it
//! trails by a frame or two. Swapping the cursor theme instead costs nothing per frame: hyprhands
//! writes a small XCursor theme (`hyprhands`) whose arrow carries a robot badge and which inherits
//! every other shape from the owner's theme, switches to it with `hyprctl setcursor`, and switches
//! back when the session ends.
//!
//! The owner's theme and size are written to a restore file before the switch, so a session that
//! dies without its `Drop` (a kill, a crash) is undone by the next session, `hyprhands doctor`, or
//! `hyprhands restore-cursor`.

use crate::draw::{Canvas, Rgba};
use crate::hypr::{self, Instance};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

pub const THEME: &str = "hyprhands";
/// Nominal sizes written into the theme; the compositor picks the one nearest size x scale.
const SIZES: [u32; 5] = [24, 32, 36, 48, 64];
/// The arrow shapes the badge goes on. Everything else (text, hand, resize) is the owner's.
const ARROWS: [&str; 5] = ["default", "left_ptr", "arrow", "top_left_arrow", "left-arrow"];

/// The cursor theme and size the owner had before a session swapped them.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub theme: String,
    pub size: u32,
}

impl Saved {
    /// What the owner's session uses: GNOME's cursor settings (which Hyprland desktops keep in
    /// step with the compositor), else the XCURSOR environment, else Hyprland's default.
    pub fn current() -> Self {
        let gsetting = |key: &str| {
            let out = std::process::Command::new("gsettings").args(["get", "org.gnome.desktop.interface", key]).output().ok()?;
            let v = String::from_utf8(out.stdout).ok()?.trim().trim_matches('\'').to_owned();
            (!v.is_empty()).then_some(v)
        };
        let theme = gsetting("cursor-theme").or_else(|| std::env::var("XCURSOR_THEME").ok()).unwrap_or_else(|| "default".into());
        let size = gsetting("cursor-size")
            .or_else(|| std::env::var("XCURSOR_SIZE").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);
        Self { theme, size }
    }

    fn encode(&self) -> String {
        format!("{} {}\n", self.size, self.theme)
    }

    fn decode(s: &str) -> Option<Self> {
        let (size, theme) = s.trim().split_once(' ')?;
        Some(Self { theme: theme.to_owned(), size: size.parse().ok()? })
    }
}

fn restore_file() -> PathBuf {
    hypr::runtime_dir().join("hyprhands-cursor")
}

fn theme_dir() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
    });
    data.join("icons").join(THEME)
}

/// How long each `cursor:invisible` state is held: Hyprland acts on that option from a 500 ms
/// timer, so a quicker toggle is never seen.
const REAPPLY_HOLD: std::time::Duration = std::time::Duration::from_millis(600);

/// `hyprctl setcursor`, made to show. Hyprland 0.56 loads the new theme but re-applies the shape
/// only when its name changes, so over an empty desktop (always `left_ptr`) the old image stays
/// however the pointer moves. Hiding and showing the cursor forces the re-apply; the pointer is
/// gone for about a second. Skipped when the owner already has the cursor hidden (it re-applies
/// when shown). Found and measured by hypr-qa's plain-hyprland profile (`qa-setcursor`).
fn set(hypr: &Instance, theme: &str, size: u32) -> Result<()> {
    let reply = hypr.request_raw(&format!("setcursor {theme} {size}"))?;
    if reply.trim() != "ok" {
        bail!("setcursor {theme} {size}: {}", reply.trim());
    }
    let hidden = hypr.query("getoption cursor:invisible").map_or(true, |v| v["bool"].as_bool().unwrap_or(v["int"].as_i64().unwrap_or(0) != 0));
    if !hidden {
        for v in [true, false] {
            set_invisible(hypr, v)?;
            std::thread::sleep(REAPPLY_HOLD);
        }
    }
    Ok(())
}

/// The runtime value of `cursor:invisible`, never the owner's config file.
fn set_invisible(hypr: &Instance, v: bool) -> Result<()> {
    let req = if hypr.lua() { format!("eval hl.config({{ cursor = {{ invisible = {v} }} }})") } else { format!("keyword cursor:invisible {v}") };
    let reply = hypr.request_raw(&req)?;
    if reply.trim() != "ok" {
        bail!("{req}: {}", reply.trim());
    }
    Ok(())
}

/// Put back a cursor a session left behind, if one did. Returns what was restored.
pub fn restore_leftover(hypr: &Instance) -> Option<Saved> {
    let saved = Saved::decode(&std::fs::read_to_string(restore_file()).ok()?)?;
    set(hypr, &saved.theme, saved.size).ok()?;
    let _ = std::fs::remove_file(restore_file());
    Some(saved)
}

/// The robot cursor for one session; dropping it puts the owner's cursor back.
pub struct RobotCursor {
    hypr: Instance,
    saved: Saved,
}

impl RobotCursor {
    pub fn install(hypr: &Instance) -> Result<Self> {
        restore_leftover(hypr); // a dead session's swap is undone before this one saves anything
        let saved = Saved::current();
        write_theme(&theme_dir(), &saved.theme)?;
        std::fs::write(restore_file(), saved.encode()).context("writing the cursor restore file")?;
        if let Err(e) = set(hypr, THEME, saved.size) {
            let _ = std::fs::remove_file(restore_file());
            return Err(e);
        }
        Ok(Self { hypr: hypr.clone(), saved })
    }

    /// A closure that puts the owner's cursor back, for a signal handler to call.
    pub fn restorer(&self) -> impl Fn() + Send + 'static {
        let (hypr, saved) = (self.hypr.clone(), self.saved.clone());
        move || {
            if set(&hypr, &saved.theme, saved.size).is_ok() {
                let _ = std::fs::remove_file(restore_file());
            }
        }
    }
}

impl Drop for RobotCursor {
    fn drop(&mut self) {
        (self.restorer())();
    }
}

/// The theme: an index inheriting the owner's theme, and the badged arrow under every arrow name.
fn write_theme(dir: &std::path::Path, inherits: &str) -> Result<()> {
    let cursors = dir.join("cursors");
    std::fs::create_dir_all(&cursors).with_context(|| format!("creating {}", cursors.display()))?;
    let inherits = if inherits == THEME { "default" } else { inherits };
    std::fs::write(dir.join("index.theme"), format!("[Icon Theme]\nName=hyprhands\nComment=The pointer while hyprhands drives\nInherits={inherits}\n"))?;
    let file = xcursor(&SIZES.map(|s| (s, robot_arrow(s))));
    let first = cursors.join(ARROWS[0]);
    write_if_changed(&first, &file)?;
    for name in &ARROWS[1..] {
        write_if_changed(&cursors.join(name), &file)?;
    }
    Ok(())
}

fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if std::fs::read(path).ok().as_deref() != Some(bytes) {
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// The arrow at `size` px with the robot badge at its lower right; the hotspot is the tip.
fn robot_arrow(size: u32) -> (Canvas, (u32, u32)) {
    let k = size as f32 / 24.0;
    let mut c = Canvas::new(size, size);
    // A classic left-pointing arrow in a 24-unit box, tip at (1, 1).
    let arrow = [(1.0, 1.0), (1.0, 17.0), (5.0, 13.5), (8.0, 20.5), (10.6, 19.4), (7.7, 12.6), (12.6, 12.6)].map(|(x, y): (f32, f32)| (x * k, y * k));
    let outline: Vec<(f32, f32)> = grow(&arrow, 1.1 * k);
    c.polygon(&outline, Rgba(0x10, 0x10, 0x10, 0xff));
    c.polygon(&arrow, Rgba(0xff, 0xff, 0xff, 0xff));
    let (_, robot) = qoi::decode_to_vec(include_bytes!("../assets/robot-32.qoi")).expect("the robot asset decodes");
    let badge = (size as f32 * 0.58).round() as u32;
    let at = i64::from(size - badge);
    c.image(&robot, 32, 32, at, at, badge);
    let tip = k.round() as u32;
    (c, (tip, tip))
}

/// A polygon pushed outward by `d` along each vertex's averaged normal (for the outline).
fn grow(pts: &[(f32, f32)], d: f32) -> Vec<(f32, f32)> {
    let n = pts.len();
    let (cx, cy) = pts.iter().fold((0.0, 0.0), |(a, b), p| (a + p.0 / n as f32, b + p.1 / n as f32));
    pts.iter()
        .map(|&(x, y)| {
            let (dx, dy) = (x - cx, y - cy);
            let len = dx.hypot(dy).max(1e-3);
            (x + dx / len * d, y + dy / len * d)
        })
        .collect()
}

/// An XCursor file holding one image per size (premultiplied ARGB, as the format stores it).
/// One XCursor image: nominal size, and the picture with its hotspot.
type Image = (u32, (Canvas, (u32, u32)));

fn xcursor(images: &[Image]) -> Vec<u8> {
    const IMAGE: u32 = 0xfffd_0002;
    let mut out = Vec::new();
    let put = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    put(&mut out, u32::from_le_bytes(*b"Xcur"));
    put(&mut out, 16); // header size
    put(&mut out, 0x1_0000); // version
    put(&mut out, images.len() as u32);
    let mut pos = 16 + 12 * images.len() as u32;
    for (nominal, (c, _)) in images {
        put(&mut out, IMAGE);
        put(&mut out, *nominal);
        put(&mut out, pos);
        pos += 36 + 4 * c.w * c.h;
    }
    for (nominal, (c, (hx, hy))) in images {
        for v in [36, IMAGE, *nominal, 1, c.w, c.h, *hx, *hy, 0] {
            put(&mut out, v);
        }
        for px in &c.px {
            put(&mut out, *px);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_saved_cursor_round_trips_with_spaces_in_the_theme_name() {
        let s = Saved { theme: "Bibata Modern Ice".into(), size: 32 };
        assert_eq!(Saved::decode(&s.encode()), Some(s));
        assert_eq!(Saved::decode("garbage"), None);
    }

    #[test]
    fn the_arrow_has_its_tip_at_the_hotspot_and_a_badge_at_the_corner() {
        let (c, hot) = robot_arrow(48);
        assert_eq!(hot, (2, 2));
        let a = |x: u32, y: u32| c.px[(y * c.w + x) as usize] >> 24;
        assert!(a(4, 6) > 0x80, "arrow body near the tip");
        assert!(a(40, 40) > 0x80, "badge at the lower right");
        assert_eq!(a(46, 2), 0, "clear above the badge");
    }

    #[test]
    fn an_xcursor_file_has_a_toc_entry_and_an_image_per_size() {
        let imgs = [24, 48].map(|s| (s, robot_arrow(s)));
        let f = xcursor(&imgs);
        let u = |i: usize| u32::from_le_bytes(f[i..i + 4].try_into().unwrap());
        assert_eq!(&f[..4], b"Xcur");
        assert_eq!(u(12), 2);
        let (first, second) = (u(16 + 8) as usize, u(28 + 8) as usize);
        assert_eq!((u(first + 16), u(first + 20)), (24, 24));
        assert_eq!((u(second + 16), u(second + 20)), (48, 48));
        assert_eq!(f.len(), second + 36 + 4 * 48 * 48);
    }

    #[test]
    fn the_theme_inherits_the_owners_and_names_every_arrow() {
        let dir = std::env::temp_dir().join(format!("hyprhands-cursor-test-{}", std::process::id()));
        write_theme(&dir, "Adwaita").unwrap();
        assert!(std::fs::read_to_string(dir.join("index.theme")).unwrap().contains("Inherits=Adwaita"));
        for name in ARROWS {
            assert!(dir.join("cursors").join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
