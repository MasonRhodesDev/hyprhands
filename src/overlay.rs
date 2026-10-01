//! What the owner sees while hyprhands drives:
//!
//! - a tint over every monitor that shares the agent's seat (on Hyprland, all of them: it has one
//!   seat, so the owner's pointer and keyboard are the agent's), slowly pulsing, saying the desktop
//!   is in agent hands wherever the owner looks;
//! - on the driven monitor, a hazard-tape frame: diagonal stripes that keep marching, between dark
//!   rims, so it reads on any wallpaper and against any border colour, and reads as live;
//! - a ring where each click lands, and a trail when the pointer jumps to get there;
//! - a caption saying what it is doing.
//!
//! The colour is the owner's theme turned around: the complement of their active border's hue at
//! full saturation (orange against a blue theme), so it never blends into the desktop; stopped is
//! red. See `Palette`.
//!
//! Each monitor's overlay is its own Wayland connection on its own thread, so drawing never waits
//! on a capture or an input and the other way round. Every surface is a layer-shell surface on the
//! overlay layer, namespace `hyprhands`, with an empty input region: nothing it draws can take a
//! click, a key or focus. Screencopy would capture it, so the server hides the driven monitor's
//! overlay for each capture (see `set_visible`). When the process ends, the connections close and
//! the compositor removes every surface, so there is no cleanup path to get wrong.

use crate::draw::{self, Canvas, Rgba};
use crate::hypr::Monitor;
use anyhow::{Context, Result, anyhow};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

pub const NAMESPACE: &str = "hyprhands";
/// Frame thickness, logical px: a dark rim, the striped band, and a dark rim again.
const EDGE: u32 = 12;
const EDGE_RIM: u32 = 2;
const EDGE_DARK: Rgba = Rgba(0, 0, 0, 0xc0);
/// Hazard stripes: period (logical px along the edge) and how fast they march.
const STRIPE: u32 = 18;
const STRIPE_PX_PER_S: f32 = 36.0;
/// The tint pulses between these alphas, once every `PULSE`.
const TINT_LOW: f32 = 0.07;
const TINT_HIGH: f32 = 0.16;
const PULSE: Duration = Duration::from_millis(2400);
/// Animation steps: stripes and pulse are redrawn at most this often.
const ANIM_STEP: Duration = Duration::from_millis(40);
const TICK: Duration = Duration::from_millis(16);
const RING: u32 = 64;
const RING_FOR: Duration = Duration::from_millis(420);
const TRAIL_FOR: Duration = Duration::from_millis(450);
const CAPTION_FOR: Duration = Duration::from_millis(4000);
const CAPTION_PX: f32 = 18.0;
const CAPTION_PAD: f32 = 12.0;
const CAPTION_MARGIN: i32 = 28;
const CAPTION_BG: Rgba = Rgba(0x2a, 0x2c, 0x34, 0xf0);
/// The caption's border, in the mode colour: a dark panel alone vanishes on a dark page.
const CAPTION_BORDER: f32 = 1.5;
const CAPTION_FG: Rgba = Rgba(0xf2, 0xf2, 0xf2, 0xff);

/// Who has the seat, which the colours say.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Driving,
    /// The owner took over, or the panic file: hyprhands refuses input.
    Stopped,
}

/// The overlay's colours, derived from the owner's theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub driving: Rgba,
    pub stopped: Rgba,
}

impl Palette {
    pub const STOPPED: Rgba = Rgba(0xff, 0x3b, 0x4a, 0xff);
    /// Used when the theme's colour cannot be read.
    pub const FALLBACK: Rgba = Rgba(0xff, 0x8a, 0x00, 0xff);

    /// The complement of `theme`'s hue at full saturation and mid lightness: whatever the owner's
    /// accent is, the overlay is its opposite. A grey theme (no hue to turn) gets the fallback
    /// orange, and a driving colour too near the stopped red is pushed to orange so the two never
    /// read alike.
    pub fn against(theme: Option<Rgba>) -> Self {
        let driving = theme
            .and_then(|Rgba(r, g, b, _)| {
                let (h, s, _) = hsl(r, g, b);
                (s > 0.12).then(|| (h + 180.0) % 360.0)
            })
            .map(|h| {
                if !(25.0..=330.0).contains(&h) {
                    30.0
                } else {
                    h
                }
            })
            .map_or(Self::FALLBACK, |h| from_hsl(h, 1.0, 0.52));
        Self {
            driving,
            stopped: Self::STOPPED,
        }
    }

    pub fn colour(&self, m: Mode) -> Rgba {
        match m {
            Mode::Driving => self.driving,
            Mode::Stopped => self.stopped,
        }
    }
}

fn hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    );
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Rgba {
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

/// What an overlay draws on its monitor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    /// The monitor hyprhands drives: tint, frame, rings, trails, captions.
    Driven,
    /// Another monitor on the same seat: the tint only.
    Seat,
}

enum Cmd {
    Mode(Mode),
    /// Show or hide everything; the reply comes once the compositor has drawn the change.
    Visible(bool, Sender<()>),
    /// A click at a point in the monitor's logical space.
    Click(f64, f64),
    /// The pointer jumped from one point to another.
    Jump((f64, f64), (f64, f64)),
    Caption(String),
    Quit,
}

/// A cloneable way to talk to an overlay from any thread.
#[derive(Clone)]
pub struct Remote(Sender<Cmd>);

impl Remote {
    pub fn mode(&self, mode: Mode) {
        let _ = self.0.send(Cmd::Mode(mode));
    }

    pub fn click(&self, x: f64, y: f64) {
        let _ = self.0.send(Cmd::Click(x, y));
    }

    pub fn jump(&self, from: (f64, f64), to: (f64, f64)) {
        let _ = self.0.send(Cmd::Jump(from, to));
    }

    /// Take the overlay down now (a signal is ending the process).
    pub fn quit(&self) {
        let _ = self.0.send(Cmd::Quit);
    }

    pub fn caption(&self, text: impl Into<String>) {
        let _ = self.0.send(Cmd::Caption(text.into()));
    }
}

/// A handle on one monitor's overlay thread. Dropping it takes that overlay down.
pub struct Overlay {
    remote: Remote,
    thread: Option<JoinHandle<()>>,
}

impl Overlay {
    pub fn start(monitor: &Monitor, role: Role, palette: Palette) -> Result<Self> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let monitor = monitor.clone();
        let thread = std::thread::Builder::new()
            .name("hyprhands-overlay".into())
            .spawn(move || match Painter::new(monitor, role, palette) {
                Ok(mut p) => {
                    let _ = ready_tx.send(Ok(()));
                    p.run(rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("the overlay thread died starting"))??;
        Ok(Self {
            remote: Remote(tx),
            thread: Some(thread),
        })
    }

    pub fn remote(&self) -> Remote {
        self.remote.clone()
    }

    /// Show the overlay again without waiting for it to be drawn.
    pub fn show(&self) {
        let (done_tx, _) = channel();
        let _ = self.remote.0.send(Cmd::Visible(true, done_tx));
    }

    /// Show or hide the overlay and wait (up to `wait`) until the compositor has drawn it so.
    pub fn set_visible(&self, visible: bool, wait: Duration) -> bool {
        let (done_tx, done_rx) = channel();
        self.remote.0.send(Cmd::Visible(visible, done_tx)).is_ok()
            && done_rx.recv_timeout(wait).is_ok()
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let _ = self.remote.0.send(Cmd::Quit); // a Remote may outlive this handle; the thread ends anyway
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ----- the thread ---------------------------------------------------------------------------

/// One shared-memory buffer holding a painted canvas.
struct Shm {
    ptr: *mut u8,
    len: usize,
    _fd: OwnedFd,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
}

impl Drop for Shm {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: ptr/len came from the mmap in `Shm::upload` and nothing else holds them.
        unsafe {
            let _ = munmap(self.ptr.cast(), self.len);
        }
    }
}

impl Shm {
    fn upload(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, c: &Canvas) -> Result<Self> {
        let len = c.px.len() * 4;
        let fd = memfd_create("hyprhands-overlay", MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, len as u64)?;
        // SAFETY: a fresh shared mapping of the memfd we just sized.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )?
        }
        .cast::<u8>();
        // SAFETY: the mapping is `len` bytes, page aligned, and only this thread writes it.
        unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u32>(), c.px.len()) }
            .copy_from_slice(&c.px);
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            c.w as i32,
            c.h as i32,
            (c.w * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        Ok(Self {
            ptr,
            len,
            _fd: fd,
            pool,
            buffer,
        })
    }
}

/// Which screen edge a frame layer runs along; its outer side is the screen's edge.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

/// What a layer shows, drawn at whatever size the compositor configured.
#[derive(Clone, Debug, PartialEq)]
enum Content {
    Clear,
    /// The whole monitor washed in the mode colour at a pulse level (0-255), as a 1x1 buffer the
    /// viewport stretches.
    Tint(Mode, u8),
    /// One frame edge; `phase` is the stripes' march, in logical px.
    Edge(Side, Mode, u32),
    /// A click ring, `t` of the way through its animation.
    Ring(f32),
    /// A line from one point to another, in the layer's own coordinates.
    Trail((f32, f32), (f32, f32)),
    Caption(String, Mode),
}

struct Layer {
    surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    /// Only the tint has one: it stretches a 1x1 buffer over the whole monitor.
    viewport: Option<wp_viewport::WpViewport>,
    /// Configured size, logical px.
    size: Option<(u32, u32)>,
    want: Content,
    shown: Option<Content>,
    buffer: Option<Shm>,
}

/// Layer indices. The tint is created first so everything else stacks above it; a `Seat`
/// overlay has only the tint.
const TINT_L: usize = 0;
const EDGES: [(usize, Side); 4] = [
    (1, Side::Top),
    (2, Side::Bottom),
    (3, Side::Left),
    (4, Side::Right),
];
const RING_L: usize = 5;
const TRAIL_L: usize = 6;
const CAPTION_L: usize = 7;

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    /// Configures waiting to be acked: (layer index, serial, width, height).
    configures: Vec<(usize, u32, u32, u32)>,
    closed: bool,
    frame_done: bool,
}

struct Painter {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    shm: wl_shm::WlShm,
    layers: Vec<Layer>,
    role: Role,
    palette: Palette,
    /// Integer buffer scale: the monitor's scale rounded up, so text stays sharp at 1.5.
    bs: u32,
    monitor: Monitor,
    font: Option<fontdue::Font>,
    mode: Mode,
    visible: bool,
    born: Instant,
    anim_at: Instant,
    ring_since: Option<Instant>,
    trail_until: Option<Instant>,
    caption_until: Option<Instant>,
}

impl Painter {
    fn new(monitor: Monitor, role: Role, palette: Palette) -> Result<Self> {
        let conn =
            Connection::connect_to_env().context("overlay: connecting to the Wayland display")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("overlay: reading Wayland globals")?;
        let qh = queue.handle();
        let mut state = State::default();
        let compositor: wl_compositor::WlCompositor = globals
            .bind(&qh, 4..=6, ())
            .context("overlay: wl_compositor")?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("overlay: wl_shm")?;
        let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals
            .bind(&qh, 1..=4, ())
            .context("the compositor lacks zwlr_layer_shell_v1")?;
        let viewporter: wp_viewporter::WpViewporter = globals
            .bind(&qh, 1..=1, ())
            .context("the compositor lacks wp_viewporter")?;
        for g in globals.contents().clone_list() {
            if g.interface == "wl_output" {
                let o: wl_output::WlOutput =
                    globals
                        .registry()
                        .bind(g.name, g.version.min(4), &qh, state.outputs.len());
                state.outputs.push((o, None));
            }
        }
        queue.roundtrip(&mut state)?;
        let output = state
            .outputs
            .iter()
            .find(|(_, n)| n.as_deref() == Some(monitor.name.as_str()))
            .map(|(o, _)| o.clone())
            .ok_or_else(|| anyhow!("overlay: no Wayland output named {}", monitor.name))?;
        let bs = monitor.scale.ceil().max(1.0) as u32;

        use zwlr_layer_surface_v1::Anchor;
        let all = Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right;
        let corner = Anchor::Top | Anchor::Left;
        let mut specs = vec![(all, (0, 0), Content::Tint(Mode::Driving, 0))];
        if role == Role::Driven {
            for (_, side) in EDGES {
                let (anchor, size) = match side {
                    Side::Top => (Anchor::Top | Anchor::Left | Anchor::Right, (0, EDGE)),
                    Side::Bottom => (Anchor::Bottom | Anchor::Left | Anchor::Right, (0, EDGE)),
                    Side::Left => (Anchor::Left | Anchor::Top | Anchor::Bottom, (EDGE, 0)),
                    Side::Right => (Anchor::Right | Anchor::Top | Anchor::Bottom, (EDGE, 0)),
                };
                specs.push((anchor, size, Content::Edge(side, Mode::Driving, 0)));
            }
            specs.extend([
                (corner, (RING, RING), Content::Clear),
                (corner, (1, 1), Content::Clear),
                (Anchor::Bottom, (1, 1), Content::Clear),
            ]);
        }
        let empty: wl_region::WlRegion = compositor.create_region(&qh, ());
        let mut layers = vec![];
        for (i, (anchor, (w, h), want)) in specs.into_iter().enumerate() {
            let surface = compositor.create_surface(&qh, ());
            surface.set_input_region(Some(&empty)); // click-through
            let viewport = (i == TINT_L).then(|| viewporter.get_viewport(&surface, &qh, ()));
            if viewport.is_none() {
                surface.set_buffer_scale(bs as i32);
            }
            let layer = layer_shell.get_layer_surface(
                &surface,
                Some(&output),
                zwlr_layer_shell_v1::Layer::Overlay,
                NAMESPACE.into(),
                &qh,
                i,
            );
            layer.set_anchor(anchor);
            layer.set_size(w, h);
            layer.set_exclusive_zone(-1); // over everything, reserving nothing
            layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            if i == CAPTION_L {
                layer.set_margin(0, 0, CAPTION_MARGIN, 0);
            }
            surface.commit();
            layers.push(Layer {
                surface,
                layer,
                viewport,
                size: None,
                want,
                shown: None,
                buffer: None,
            });
        }
        empty.destroy();
        let now = Instant::now();
        let mut p = Self {
            conn,
            queue,
            qh,
            state,
            shm,
            layers,
            role,
            palette,
            bs,
            monitor,
            font: (role == Role::Driven).then(caption_font).flatten(),
            mode: Mode::Driving,
            visible: true,
            born: now,
            anim_at: now - ANIM_STEP,
            ring_since: None,
            trail_until: None,
            caption_until: None,
        };
        p.queue.roundtrip(&mut p.state)?;
        p.apply_configures()?;
        Ok(p)
    }

    fn apply_configures(&mut self) -> Result<()> {
        for (i, serial, w, h) in std::mem::take(&mut self.state.configures) {
            let l = &mut self.layers[i];
            l.layer.ack_configure(serial);
            if l.size != Some((w, h)) {
                l.size = Some((w, h));
                l.shown = None; // repaint at the new size
                if let Some(v) = &l.viewport {
                    v.set_destination(w as i32, h as i32);
                }
            }
        }
        self.paint_all()
    }

    /// Paint every configured layer whose content changed since it was last drawn.
    fn paint_all(&mut self) -> Result<()> {
        for i in 0..self.layers.len() {
            let want = if self.visible {
                self.layers[i].want.clone()
            } else {
                Content::Clear
            };
            let l = &self.layers[i];
            let Some((w, h)) = l.size else { continue };
            if l.shown.as_ref() == Some(&want) {
                continue;
            }
            let canvas = if l.viewport.is_some() {
                self.render_tint(&want)
            } else {
                self.render(&want, w, h)
            };
            let buf = Shm::upload(&self.shm, &self.qh, &canvas)?;
            let l = &mut self.layers[i];
            l.surface.attach(Some(&buf.buffer), 0, 0);
            l.surface
                .damage_buffer(0, 0, canvas.w as i32, canvas.h as i32);
            l.surface.commit();
            l.buffer = Some(buf); // the previous buffer drops only after the new one is committed
            l.shown = Some(want);
        }
        self.conn.flush()?;
        Ok(())
    }

    /// The tint's single pixel; the viewport stretches it over the monitor.
    fn render_tint(&self, content: &Content) -> Canvas {
        match content {
            Content::Tint(m, level) => {
                let a = TINT_LOW + (TINT_HIGH - TINT_LOW) * f32::from(*level) / 255.0;
                Canvas::filled(1, 1, self.palette.colour(*m).with_alpha(a))
            }
            _ => Canvas::new(1, 1),
        }
    }

    fn render(&self, content: &Content, w: u32, h: u32) -> Canvas {
        let s = self.bs as f32;
        let (bw, bh) = (w * self.bs, h * self.bs);
        match content {
            Content::Clear | Content::Tint(..) => Canvas::new(bw, bh),
            Content::Edge(side, mode, phase) => {
                edge_canvas(*side, self.palette.colour(*mode), bw, bh, self.bs, *phase)
            }
            Content::Ring(t) => {
                let mut c = Canvas::new(bw, bh);
                let (half, t) = (bw as f32 / 2.0, t.clamp(0.0, 1.0));
                let colour = self.palette.colour(self.mode).with_alpha(1.0 - t);
                c.disc(half, half, 4.0 * s, colour);
                c.ring(half, half, (6.0 + 22.0 * t) * s, 3.0 * s, colour);
                c
            }
            Content::Trail(a, b) => {
                let mut c = Canvas::new(bw, bh);
                let colour = self.palette.colour(self.mode).with_alpha(0.55);
                c.line((a.0 * s, a.1 * s), (b.0 * s, b.1 * s), 3.0 * s, colour);
                c.disc(a.0 * s, a.1 * s, 3.0 * s, colour);
                c
            }
            Content::Caption(text, mode) => {
                let mut c = Canvas::new(bw, bh);
                let Some(font) = &self.font else { return c };
                let colour = self.palette.colour(*mode);
                let b = CAPTION_BORDER * s;
                c.rounded_rect(
                    0.0,
                    0.0,
                    bw as f32,
                    bh as f32,
                    10.0 * s,
                    colour.with_alpha(0.85),
                );
                c.rounded_rect(
                    b,
                    b,
                    bw as f32 - 2.0 * b,
                    bh as f32 - 2.0 * b,
                    10.0 * s - b,
                    CAPTION_BG,
                );
                c.rounded_rect(
                    CAPTION_PAD * s * 0.5,
                    (bh as f32 - 20.0 * s) / 2.0,
                    4.0 * s,
                    20.0 * s,
                    2.0 * s,
                    colour,
                );
                let px = CAPTION_PX * s;
                let (asc, desc) = font
                    .horizontal_line_metrics(px)
                    .map_or((px * 0.8, -px * 0.2), |m| (m.ascent, m.descent));
                let baseline = (bh as f32 + asc + desc) / 2.0;
                c.text(
                    font,
                    px,
                    CAPTION_PAD * s + 6.0 * s,
                    baseline,
                    text,
                    CAPTION_FG,
                );
                c
            }
        }
    }

    /// Move a top-left-anchored layer so its top-left is at (x, y), logical px on the monitor.
    fn place(&mut self, i: usize, x: i32, y: i32) {
        self.layers[i].layer.set_margin(y.max(0), 0, 0, x.max(0));
        self.layers[i].surface.commit();
    }

    fn resize(&mut self, i: usize, w: u32, h: u32) {
        self.layers[i].layer.set_size(w.max(1), h.max(1));
        self.layers[i].surface.commit(); // the compositor answers with a configure at the new size
    }

    fn set(&mut self, i: usize, c: Content) {
        self.layers[i].want = c;
    }

    fn animate(&mut self) {
        let now = Instant::now();
        // The pulse and the marching stripes: what makes the overlay read as live.
        if now.duration_since(self.anim_at) >= ANIM_STEP {
            self.anim_at = now;
            let t = now.duration_since(self.born).as_secs_f32();
            let level =
                ((t / PULSE.as_secs_f32() * std::f32::consts::TAU).sin() * 0.5 + 0.5) * 255.0;
            self.set(TINT_L, Content::Tint(self.mode, level.round() as u8));
            if self.role == Role::Driven {
                let phase = (t * STRIPE_PX_PER_S) as u32 % STRIPE;
                for (i, side) in EDGES {
                    self.set(i, Content::Edge(side, self.mode, phase));
                }
            }
        }
        if self.role != Role::Driven {
            return;
        }
        if let Some(start) = self.ring_since {
            let t = now.duration_since(start).as_secs_f32() / RING_FOR.as_secs_f32();
            if t >= 1.0 {
                self.ring_since = None;
                self.set(RING_L, Content::Clear);
            } else {
                // A dozen steps are plenty for 420 ms and keep repaints rare.
                self.set(RING_L, Content::Ring((t * 12.0).floor() / 12.0));
            }
        }
        if self.trail_until.is_some_and(|u| now >= u) {
            self.trail_until = None;
            self.set(TRAIL_L, Content::Clear);
        }
        if self.caption_until.is_some_and(|u| now >= u) && self.mode == Mode::Driving {
            self.caption_until = None;
            self.set(CAPTION_L, Content::Clear);
        }
    }

    fn command(&mut self, cmd: Cmd) -> bool {
        let driven = self.role == Role::Driven;
        match cmd {
            Cmd::Quit => return false,
            Cmd::Mode(m) => {
                self.mode = m;
                self.anim_at = Instant::now() - ANIM_STEP; // repaint tint and edges in the new colour now
                if driven && let Content::Caption(text, _) = self.layers[CAPTION_L].want.clone() {
                    self.set(CAPTION_L, Content::Caption(text, m));
                }
            }
            Cmd::Visible(v, done) => {
                self.visible = v;
                let _ = self.paint_all();
                if !v {
                    let _ = self.wait_drawn(Duration::from_millis(100));
                }
                let _ = done.send(());
            }
            Cmd::Click(x, y) if driven => {
                let half = (RING / 2) as i32;
                self.place(RING_L, x.round() as i32 - half, y.round() as i32 - half);
                self.ring_since = Some(Instant::now());
                self.set(RING_L, Content::Ring(0.0));
            }
            Cmd::Jump(a, b) if driven => {
                let pad = 4.0;
                let (x0, y0) = ((a.0.min(b.0) - pad).round(), (a.1.min(b.1) - pad).round());
                let (w, h) = ((a.0 - b.0).abs() + 2.0 * pad, (a.1 - b.1).abs() + 2.0 * pad);
                if w.max(h) > 24.0 {
                    self.place(TRAIL_L, x0 as i32, y0 as i32);
                    self.resize(TRAIL_L, w.ceil() as u32, h.ceil() as u32);
                    let local = |p: (f64, f64)| ((p.0 - x0) as f32, (p.1 - y0) as f32);
                    self.set(TRAIL_L, Content::Trail(local(a), local(b)));
                    self.trail_until = Some(Instant::now() + TRAIL_FOR);
                }
            }
            Cmd::Caption(text) if driven => {
                if let Some(font) = &self.font {
                    let max = (self.monitor.width as f32 - 80.0).max(120.0);
                    let text = draw::fit(font, CAPTION_PX, &text, max - 2.0 * CAPTION_PAD - 6.0);
                    let w = draw::text_width(font, CAPTION_PX, &text) + 2.0 * CAPTION_PAD + 6.0;
                    let h = CAPTION_PX + 2.0 * CAPTION_PAD;
                    self.resize(CAPTION_L, w.ceil() as u32, h.ceil() as u32);
                    self.set(CAPTION_L, Content::Caption(text, self.mode));
                    self.caption_until = Some(Instant::now() + CAPTION_FOR);
                }
            }
            Cmd::Click(..) | Cmd::Jump(..) | Cmd::Caption(_) => {} // a Seat overlay only tints
        }
        true
    }

    /// Commit a frame callback and wait until the compositor has drawn this client's last commits.
    fn wait_drawn(&mut self, limit: Duration) -> Result<()> {
        let Some(l) = self.layers.iter().find(|l| l.size.is_some()) else {
            return Ok(());
        };
        self.state.frame_done = false;
        l.surface.frame(&self.qh, ());
        l.surface.commit();
        self.conn.flush()?;
        let end = Instant::now() + limit;
        while !self.state.frame_done && Instant::now() < end {
            self.pump(Duration::from_millis(4))?;
        }
        Ok(())
    }

    /// Read and dispatch Wayland events, waiting at most `timeout` for some to arrive.
    fn pump(&mut self, timeout: Duration) -> Result<()> {
        self.queue.flush()?;
        if let Some(guard) = self.queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )];
            let ts = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: timeout.as_nanos() as i64,
            };
            if rustix::event::poll(&mut fds, Some(&ts)).unwrap_or(0) > 0 {
                let _ = guard.read();
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        if !self.state.configures.is_empty() {
            self.apply_configures()?;
        }
        Ok(())
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            loop {
                match rx.try_recv() {
                    Ok(cmd) => {
                        if !self.command(cmd) {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if self.visible {
                self.animate();
            }
            if self.paint_all().is_err() || self.pump(TICK).is_err() || self.state.closed {
                return;
            }
        }
    }
}

/// One frame edge in buffer pixels: a dark rim at the screen's edge, a band of diagonal hazard
/// stripes (the colour and a dark shade of it) offset by `phase` so they march, and a dark rim on
/// the inside. It reads against a light or a dark background alike, and as moving.
fn edge_canvas(side: Side, colour: Rgba, bw: u32, bh: u32, bs: u32, phase: u32) -> Canvas {
    let mut c = Canvas::new(bw, bh);
    let (rim, depth, period) = (EDGE_RIM * bs, EDGE * bs, STRIPE * bs);
    let shade = Rgba(
        (u32::from(colour.0) * 2 / 5) as u8,
        (u32::from(colour.1) * 2 / 5) as u8,
        (u32::from(colour.2) * 2 / 5) as u8,
        0xff,
    );
    let (bright, dark, edge_dark) = (
        colour.premultiplied(),
        shade.premultiplied(),
        EDGE_DARK.premultiplied(),
    );
    for y in 0..bh {
        for x in 0..bw {
            // `d`: in from the screen's edge; `along`: along the edge. Both in buffer px.
            let (d, along) = match side {
                Side::Top => (y, x),
                Side::Bottom => (bh - 1 - y, x),
                Side::Left => (x, y),
                Side::Right => (bw - 1 - x, y),
            };
            c.px[(y * bw + x) as usize] = if d < rim || d >= depth - rim {
                edge_dark
            } else if (along + d + phase * bs) % period < period / 2 {
                bright
            } else {
                dark
            };
        }
    }
    c
}

/// The system's bold sans for captions, found through fontconfig; none means no captions.
fn caption_font() -> Option<fontdue::Font> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif:bold"])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    let bytes = std::fs::read(path.trim()).ok()?;
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
}

// ----- event plumbing -----------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs[*index].1 = Some(name);
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => state.configures.push((*index, serial, width, height)),
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_done = true;
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_turns_the_theme_around() {
        // The owner's #a4c9fe (blue, hue ~215) -> its complement, an orange.
        let p = Palette::against(Some(Rgba(0xa4, 0xc9, 0xfe, 0xff)));
        let (h, s, _) = hsl(p.driving.0, p.driving.1, p.driving.2);
        assert!((30.0..=40.0).contains(&h) && s > 0.95, "hue {h} sat {s}");
        // A grey theme has no hue to turn: the fallback orange.
        assert_eq!(
            Palette::against(Some(Rgba(0x8d, 0x91, 0x99, 0xff))).driving,
            Palette::FALLBACK
        );
        assert_eq!(Palette::against(None).driving, Palette::FALLBACK);
        // A teal theme's complement would be red, the stopped colour: pushed to orange instead.
        let red = Palette::against(Some(Rgba(0x00, 0xc0, 0xc0, 0xff)));
        assert!(hsl(red.driving.0, red.driving.1, red.driving.2).0 >= 25.0);
    }

    #[test]
    fn an_edge_has_dark_rims_and_two_tone_stripes_that_march() {
        let colour = Rgba(0xff, 0x8a, 0x00, 0xff);
        let c = edge_canvas(Side::Top, colour, 64, EDGE, 1, 0);
        let at = |c: &Canvas, x: u32, y: u32| c.px[(y * c.w + x) as usize];
        assert_eq!(
            at(&c, 10, 0),
            EDGE_DARK.premultiplied(),
            "outer rim at the screen edge"
        );
        assert_eq!(at(&c, 10, EDGE - 1), EDGE_DARK.premultiplied(), "inner rim");
        let band: std::collections::HashSet<u32> = (0..64).map(|x| at(&c, x, EDGE / 2)).collect();
        assert_eq!(band.len(), 2, "two-tone stripes in the band");
        let moved = edge_canvas(Side::Top, colour, 64, EDGE, 1, STRIPE / 2);
        assert_ne!(
            at(&c, 0, EDGE / 2),
            at(&moved, 0, EDGE / 2),
            "half a period later the stripe has swapped"
        );
        let r = edge_canvas(Side::Right, colour, EDGE, 8, 1, 0);
        assert_eq!(
            at(&r, EDGE - 1, 3),
            EDGE_DARK.premultiplied(),
            "a right edge's outer rim is its last column"
        );
    }
}
