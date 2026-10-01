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
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

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

/// Bumped whenever the badging changes, so a theme written by an older hyprhands is rewritten.
const BADGE_VERSION: u32 = 2;

/// The theme: every shape of the owner's theme (its own and those it inherits) with the robot
/// stamped on, an index inheriting the owner's theme for anything missed, and the drawn robot
/// arrow when the owner has no cursor theme at all. Skipped when nothing changed since last time.
fn write_theme(dir: &Path, base: &str) -> Result<()> {
    write_theme_from(dir, base, &icon_paths())
}

fn write_theme_from(dir: &Path, base: &str, search: &[PathBuf]) -> Result<()> {
    let base = if base == THEME { "default" } else { base };
    let shapes = base_shapes(base, search);
    let newest = shapes.values().filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).max();
    let stamp = format!("{BADGE_VERSION}\n{base}\n{}\n{newest:?}\n", shapes.len());
    if std::fs::read_to_string(dir.join(".stamp")).ok().as_deref() == Some(stamp.as_str()) {
        return Ok(());
    }
    let cursors = dir.join("cursors");
    let _ = std::fs::remove_dir_all(&cursors); // ours alone: rebuilt from the owner's theme
    std::fs::create_dir_all(&cursors).with_context(|| format!("creating {}", cursors.display()))?;
    std::fs::write(dir.join("index.theme"), format!("[Icon Theme]\nName=hyprhands\nComment=The pointer while hyprhands drives\nInherits={base}\n"))?;
    let (_, robot) = qoi::decode_to_vec(include_bytes!("../assets/robot-32.qoi")).context("the robot asset")?;
    // Aliases (mostly symlinks in a theme) share one badged file: the first name written for a
    // file is the real one, the rest link to it.
    let mut written: HashMap<PathBuf, String> = HashMap::new();
    for (name, path) in &shapes {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if let Some(first) = written.get(&target) {
            let _ = std::os::unix::fs::symlink(first, cursors.join(name));
            continue;
        }
        let Some(mut images) = std::fs::read(&target).ok().and_then(|b| xcursor_read(&b)) else { continue };
        for img in &mut images {
            badge(img, &robot);
        }
        std::fs::write(cursors.join(name), xcursor(&images)).with_context(|| format!("writing cursor {name}"))?;
        written.insert(target, name.clone());
    }
    if !ARROWS.iter().any(|a| shapes.contains_key(*a)) {
        let file = xcursor(&SIZES.map(|s| {
            let (canvas, hot) = robot_arrow(s);
            XImage { nominal: s, hot, delay: 0, canvas }
        }));
        for name in ARROWS {
            std::fs::write(cursors.join(name), &file)?;
        }
    }
    std::fs::write(dir.join(".stamp"), stamp)?;
    Ok(())
}

/// Where cursor themes live, in libXcursor's order: `$XCURSOR_PATH`, else its default list.
fn icon_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let list = std::env::var("XCURSOR_PATH").unwrap_or_else(|_| "~/.local/share/icons:~/.icons:/usr/share/icons:/usr/share/pixmaps".into());
    list.split(':').filter(|p| !p.is_empty()).map(|p| PathBuf::from(p.replacen('~', &home, 1))).collect()
}

/// Every cursor shape a theme provides, by name: its own `cursors/` first, then the themes it
/// inherits, breadth first, the first found winning (as Hyprland and libXcursor resolve them).
fn base_shapes(theme: &str, search: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut shapes = BTreeMap::new();
    let mut queue = std::collections::VecDeque::from([theme.to_owned()]);
    let mut seen = std::collections::HashSet::new();
    while let Some(t) = queue.pop_front() {
        if t == THEME || !seen.insert(t.clone()) {
            continue;
        }
        for root in search {
            let dir = root.join(&t);
            if let Ok(entries) = std::fs::read_dir(dir.join("cursors")) {
                for e in entries.flatten() {
                    if let Some(name) = e.file_name().to_str() {
                        shapes.entry(name.to_owned()).or_insert_with(|| e.path());
                    }
                }
            }
            if let Ok(index) = std::fs::read_to_string(dir.join("index.theme")) {
                for line in index.lines() {
                    if let Some(list) = line.trim().strip_prefix("Inherits").and_then(|r| r.trim_start().strip_prefix('=')) {
                        queue.extend(list.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()));
                    }
                }
            }
        }
    }
    shapes
}

/// Stamp the robot on one cursor image: at the lower right of what the image actually draws (an
/// I-beam is a thin strip in the middle of its square), kept inside the image, hotspot untouched.
fn badge(img: &mut XImage, robot: &[u8]) {
    let c = &mut img.canvas;
    let side = ((img.nominal as f32 * 0.58).round() as u32).min(c.w).min(c.h).max(1);
    let drawn = (0..c.h).flat_map(|y| (0..c.w).map(move |x| (x, y))).filter(|&(x, y)| c.px[(y * c.w + x) as usize] >> 24 > 0x40);
    let (mut x1, mut y1) = (0u32, 0u32);
    let mut any = false;
    for (x, y) in drawn {
        any = true;
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    if !any {
        return;
    }
    let overlap = side * 2 / 5;
    let x = (x1 + 1 + overlap).saturating_sub(side).min(c.w - side);
    let y = (y1 + 1 + overlap).saturating_sub(side).min(c.h - side);
    c.image(robot, 32, 32, i64::from(x), i64::from(y), side);
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

/// One XCursor image: its nominal size, hotspot, frame delay (animated shapes), and pixels.
#[derive(Clone)]
struct XImage {
    nominal: u32,
    hot: (u32, u32),
    delay: u32,
    canvas: Canvas,
}

const XCURSOR_IMAGE: u32 = 0xfffd_0002;

/// An XCursor file holding `images` (premultiplied ARGB, as the format stores it).
fn xcursor(images: &[XImage]) -> Vec<u8> {
    let mut out = Vec::new();
    let put = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    put(&mut out, u32::from_le_bytes(*b"Xcur"));
    put(&mut out, 16); // header size
    put(&mut out, 0x1_0000); // version
    put(&mut out, images.len() as u32);
    let mut pos = 16 + 12 * images.len() as u32;
    for img in images {
        put(&mut out, XCURSOR_IMAGE);
        put(&mut out, img.nominal);
        put(&mut out, pos);
        pos += 36 + 4 * img.canvas.w * img.canvas.h;
    }
    for img in images {
        let c = &img.canvas;
        for v in [36, XCURSOR_IMAGE, img.nominal, 1, c.w, c.h, img.hot.0, img.hot.1, img.delay] {
            put(&mut out, v);
        }
        for px in &c.px {
            put(&mut out, *px);
        }
    }
    out
}

/// The images of an XCursor file, in file order (an animated shape's frames stay in sequence).
fn xcursor_read(b: &[u8]) -> Option<Vec<XImage>> {
    let u = |i: usize| b.get(i..i + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    if b.get(..4)? != b"Xcur" {
        return None;
    }
    let ntoc = u(12)? as usize;
    let mut images = vec![];
    for t in 0..ntoc {
        let e = 16 + 12 * t;
        if u(e)? != XCURSOR_IMAGE {
            continue;
        }
        let pos = u(e + 8)? as usize;
        let (w, h) = (u(pos + 16)?, u(pos + 20)?);
        if w == 0 || h == 0 || w > 1024 || h > 1024 {
            return None;
        }
        let px: Option<Vec<u32>> = (0..(w * h) as usize).map(|i| u(pos + 36 + 4 * i)).collect();
        images.push(XImage {
            nominal: u(pos + 8)?,
            hot: (u(pos + 24)?, u(pos + 28)?),
            delay: u(pos + 32)?,
            canvas: Canvas { w, h, px: px? },
        });
    }
    (!images.is_empty()).then_some(images)
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
        let imgs = [24, 48].map(|s| {
            let (canvas, hot) = robot_arrow(s);
            XImage { nominal: s, hot, delay: 0, canvas }
        });
        let f = xcursor(&imgs);
        let u = |i: usize| u32::from_le_bytes(f[i..i + 4].try_into().unwrap());
        assert_eq!(&f[..4], b"Xcur");
        assert_eq!(u(12), 2);
        let (first, second) = (u(16 + 8) as usize, u(28 + 8) as usize);
        assert_eq!((u(first + 16), u(first + 20)), (24, 24));
        assert_eq!((u(second + 16), u(second + 20)), (48, 48));
        assert_eq!(f.len(), second + 36 + 4 * 48 * 48);
    }

    fn img(nominal: u32, w: u32, h: u32, fill: impl Fn(u32, u32) -> bool, delay: u32) -> XImage {
        let mut canvas = Canvas::new(w, h);
        for y in 0..h {
            for x in 0..w {
                if fill(x, y) {
                    canvas.px[(y * w + x) as usize] = 0xff20_2020;
                }
            }
        }
        XImage { nominal, hot: (3, 4), delay, canvas }
    }

    /// A search root with theme `base` (inheriting `parent`): an arrow with an alias, and an
    /// animated thin I-beam; `parent` adds a hand and its own `text`, which `base` shadows.
    fn fake_themes(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hyprhands-cursor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (base, parent) = (root.join("base/cursors"), root.join("parent/cursors"));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(root.join("base/index.theme"), "[Icon Theme]\nInherits = parent\n").unwrap();
        std::fs::write(base.join("left_ptr"), xcursor(&[img(24, 24, 24, |x, y| x < 12 && y < 16, 0)])).unwrap();
        std::os::unix::fs::symlink("left_ptr", base.join("arrow")).unwrap();
        let beam = |d| img(24, 24, 24, |x, _| (11..13).contains(&x), d);
        std::fs::write(base.join("text"), xcursor(&[beam(40), beam(60)])).unwrap();
        std::fs::write(parent.join("pointer"), xcursor(&[img(24, 24, 24, |x, y| x > 4 && y > 4, 0)])).unwrap();
        std::fs::write(parent.join("text"), xcursor(&[img(24, 24, 24, |_, _| true, 0)])).unwrap();
        root
    }

    #[test]
    fn every_shape_of_the_owners_theme_is_badged_aliases_linked_frames_kept() {
        let root = fake_themes("all");
        let out = root.join("out");
        write_theme_from(&out, "base", std::slice::from_ref(&root)).unwrap();
        let read = |n: &str| xcursor_read(&std::fs::read(out.join("cursors").join(n)).unwrap()).unwrap();
        assert!(std::fs::read_to_string(out.join("index.theme")).unwrap().contains("Inherits=base"));
        let link = |n: &str| std::fs::symlink_metadata(out.join("cursors").join(n)).unwrap().file_type().is_symlink();
        assert!(link("arrow") != link("left_ptr"), "an alias stays an alias: one file, one link to it");
        assert_eq!(read("arrow").len(), 1);
        let text = read("text");
        assert_eq!(text.iter().map(|i| i.delay).collect::<Vec<_>>(), vec![40, 60], "animated frames and timing kept");
        assert_eq!(text[0].hot, (3, 4), "hotspot untouched");
        // The beam (columns 11-12) got a badge right beside it, not only in the far corner.
        let px = &text[0].canvas;
        let beside = (13..20).any(|x| (12..24).any(|y| px.px[(y * 24 + x) as usize] >> 24 > 0x80 && px.px[(y * 24 + x) as usize] != 0xff20_2020));
        assert!(beside, "badge next to the I-beam");
        assert!(out.join("cursors/pointer").exists(), "inherited shapes are badged too");
        assert_eq!(read("text").len(), 2, "the theme's own text wins over the parent's");
        // Nothing changed: the stamp skips the rewrite.
        let before = std::fs::metadata(out.join("cursors/text")).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_theme_from(&out, "base", std::slice::from_ref(&root)).unwrap();
        assert_eq!(std::fs::metadata(out.join("cursors/text")).unwrap().modified().unwrap(), before);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn with_no_cursor_theme_at_all_the_drawn_robot_arrow_stands_in() {
        let root = std::env::temp_dir().join(format!("hyprhands-cursor-none-{}", std::process::id()));
        write_theme_from(&root.join("out"), "missing", std::slice::from_ref(&root)).unwrap();
        for name in ARROWS {
            assert!(root.join("out/cursors").join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
