//! What the owner sees while hyprhands drives, styled by a `Theme` (8-bit construction site by
//! default; see theme.rs):
//!
//! - on the driven monitor, a tape frame: two-tone diagonal stripes that keep marching, so it
//!   reads as live and on any wallpaper;
//! - a ripple where each click lands, like a drop in water: a splash, then rings spreading out;
//! - a tail along each jump of the pointer, drawn from where it was to where it went and fading
//!   from its end, in its own colour;
//! - a caption saying what it is doing;
//! - optionally (`serve --tint`), a wash over every monitor that shares the agent's seat (on
//!   Hyprland, all of them: it has one seat), slowly pulsing.
//!
//! A theme with `pixel` > 1 is drawn as chunky pixels: each layer is rendered at that fraction of
//! its size, its alpha ordered-dithered (so fades become dither patterns, as on 8-bit hardware),
//! and scaled back up by whole pixels.
//!
//! Each monitor's overlay is its own Wayland connection on its own thread, so drawing never waits
//! on a capture or an input and the other way round. Every surface is a layer-shell surface on the
//! overlay layer, namespace `hyprhands`, with an empty input region: nothing it draws can take a
//! click, a key or focus. Screencopy would capture it, so the server hides the driven monitor's
//! overlay for each capture (see `set_visible`). When the process ends, the connections close and
//! the compositor removes every surface, so there is no cleanup path to get wrong.

use crate::draw::{self, Canvas, Rgba};
use crate::hypr::Monitor;
pub use crate::theme::Mode;
use crate::theme::Theme;
use anyhow::{Context, Result, anyhow};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

pub const NAMESPACE: &str = "hyprhands";
/// Frame thickness and the rim on each side of the tape (when the theme has one), logical px.
const EDGE: u32 = 12;
const EDGE_RIM: f32 = 2.0;
/// Stripes: period along the edge, logical px, and how fast they march.
const STRIPE: f32 = 18.0;
const STRIPE_PX_PER_S: f32 = 36.0;
/// The tint pulses between these alphas, once every `PULSE`.
const TINT_LOW: f32 = 0.07;
const TINT_HIGH: f32 = 0.16;
const PULSE: Duration = Duration::from_millis(2400);
/// The frame's march and the pulse are redrawn at most this often; the ripple and tail at most
/// every `MOTION_STEP`.
const ANIM_STEP: Duration = Duration::from_millis(40);
const MOTION_STEP: Duration = Duration::from_millis(33);
const TICK: Duration = Duration::from_millis(16);
/// The ripple: its layer (logical px square), each ring's life, the gap between rings, how many.
const RIPPLE: u32 = 136;
const RIPPLE_RADIUS: f32 = 60.0;
const RING_LIFE: f32 = 0.72;
const RING_GAP: f32 = 0.14;
const RINGS: u32 = 3;
/// The tail: drawn out over `TAIL_DRAW`, then faded from its end over `TAIL_FADE` (seconds).
const TAIL_DRAW: f32 = 0.12;
const TAIL_FADE: f32 = 0.42;
const TAIL_PAD: f32 = 10.0;
const CAPTION_FOR: Duration = Duration::from_millis(4000);
const CAPTION_PAD: f32 = 12.0;
const CAPTION_MARGIN: i32 = 28;
const CAPTION_BORDER: f32 = 2.0;

/// What an overlay draws on its monitor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    /// The monitor hyprhands drives: frame, ripples, tails, captions (and the tint, if asked).
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
    /// `tint`: wash the monitor too (a `Seat` overlay is only ever started with it on).
    pub fn start(monitor: &Monitor, role: Role, theme: Theme, tint: bool) -> Result<Self> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let monitor = monitor.clone();
        let thread = std::thread::Builder::new().name("hyprhands-overlay".into()).spawn(move || match Painter::new(monitor, role, theme, tint) {
            Ok(mut p) => {
                let _ = ready_tx.send(Ok(()));
                p.run(rx);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        ready_rx.recv().map_err(|_| anyhow!("the overlay thread died starting"))??;
        Ok(Self { remote: Remote(tx), thread: Some(thread) })
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
        self.remote.0.send(Cmd::Visible(visible, done_tx)).is_ok() && done_rx.recv_timeout(wait).is_ok()
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
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, MapFlags::SHARED, &fd, 0)? }.cast::<u8>();
        // SAFETY: the mapping is `len` bytes, page aligned, and only this thread writes it.
        unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u32>(), c.px.len()) }.copy_from_slice(&c.px);
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, c.w as i32, c.h as i32, (c.w * 4) as i32, wl_shm::Format::Argb8888, qh, ());
        Ok(Self { ptr, len, _fd: fd, pool, buffer })
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
    /// The whole monitor washed at a pulse level (0-255), as a 1x1 buffer the viewport stretches.
    Tint(Mode, u8),
    /// One frame edge; `phase` is the stripes' march, in logical px.
    Edge(Side, Mode, u32),
    /// A click ripple, `ms` milliseconds in (quantised to the motion step).
    Ripple(u32),
    /// A jump's tail from `a` to `b` (layer-local logical px), `ms` milliseconds in.
    Tail((f32, f32), (f32, f32), u32),
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

/// A jump being drawn: its endpoints in the tail layer (logical px) and when it started.
type Jump = ((f32, f32), (f32, f32), Instant);

/// Layer indices. The tint is created first so everything else stacks above it; a `Seat`
/// overlay has only the tint.
const TINT_L: usize = 0;
const EDGES: [(usize, Side); 4] = [(1, Side::Top), (2, Side::Bottom), (3, Side::Left), (4, Side::Right)];
const RIPPLE_L: usize = 5;
const TAIL_L: usize = 6;
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
    theme: Theme,
    /// Whether this monitor is washed (off by default: the frame says it on the driven monitor).
    tint: bool,
    /// Integer buffer scale: the monitor's scale rounded up, so text stays sharp at 1.5.
    bs: u32,
    monitor: Monitor,
    font: Option<fontdue::Font>,
    mode: Mode,
    visible: bool,
    born: Instant,
    anim_at: Instant,
    motion_at: Instant,
    ripple_since: Option<Instant>,
    /// The current jump's endpoints in the tail layer, and when it started.
    tail: Option<Jump>,
    caption_until: Option<Instant>,
}

impl Painter {
    fn new(monitor: Monitor, role: Role, theme: Theme, tint: bool) -> Result<Self> {
        let conn = Connection::connect_to_env().context("overlay: connecting to the Wayland display")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("overlay: reading Wayland globals")?;
        let qh = queue.handle();
        let mut state = State::default();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).context("overlay: wl_compositor")?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("overlay: wl_shm")?;
        let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 =
            globals.bind(&qh, 1..=4, ()).context("the compositor lacks zwlr_layer_shell_v1")?;
        let viewporter: wp_viewporter::WpViewporter = globals.bind(&qh, 1..=1, ()).context("the compositor lacks wp_viewporter")?;
        for g in globals.contents().clone_list() {
            if g.interface == "wl_output" {
                let o: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, state.outputs.len());
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
        let mut specs = vec![(all, (0, 0), Content::Clear)];
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
                (corner, (RIPPLE, RIPPLE), Content::Clear),
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
            let layer = layer_shell.get_layer_surface(&surface, Some(&output), zwlr_layer_shell_v1::Layer::Overlay, NAMESPACE.into(), &qh, i);
            layer.set_anchor(anchor);
            layer.set_size(w, h);
            layer.set_exclusive_zone(-1); // over everything, reserving nothing
            layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            if i == CAPTION_L {
                layer.set_margin(0, 0, CAPTION_MARGIN, 0);
            }
            surface.commit();
            layers.push(Layer { surface, layer, viewport, size: None, want, shown: None, buffer: None });
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
            theme,
            tint,
            bs,
            monitor,
            font: (role == Role::Driven).then(caption_font).flatten(),
            mode: Mode::Driving,
            visible: true,
            born: now,
            anim_at: now - ANIM_STEP,
            motion_at: now,
            ripple_since: None,
            tail: None,
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
            let want = if self.visible { self.layers[i].want.clone() } else { Content::Clear };
            let l = &self.layers[i];
            let Some((w, h)) = l.size else { continue };
            if l.shown.as_ref() == Some(&want) {
                continue;
            }
            let canvas = if l.viewport.is_some() { self.render_tint(&want) } else { self.paint(&want, w, h) };
            let buf = Shm::upload(&self.shm, &self.qh, &canvas)?;
            let l = &mut self.layers[i];
            l.surface.attach(Some(&buf.buffer), 0, 0);
            l.surface.damage_buffer(0, 0, canvas.w as i32, canvas.h as i32);
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
                let colour = if *m == Mode::Driving { self.theme.tint } else { self.theme.stopped[0] };
                Canvas::filled(1, 1, colour.with_alpha(a))
            }
            _ => Canvas::new(1, 1),
        }
    }

    /// A layer's buffer: drawn directly at buffer scale for a smooth theme; for an 8-bit one,
    /// drawn at one canvas pixel per theme pixel, dithered, and scaled up by whole pixels.
    fn paint(&self, content: &Content, w: u32, h: u32) -> Canvas {
        let (bw, bh) = (w * self.bs, h * self.bs);
        let p = self.theme.pixel;
        if p <= 1 {
            return self.render(content, bw, bh, self.bs as f32);
        }
        let small = self.render(content, w.div_ceil(p), h.div_ceil(p), 1.0 / p as f32);
        upscale(&dither(small), p * self.bs, bw, bh)
    }

    /// Draw `content` on a `cw` x `ch` canvas where one logical px is `u` canvas px.
    fn render(&self, content: &Content, cw: u32, ch: u32, u: f32) -> Canvas {
        let t = &self.theme;
        match content {
            Content::Clear | Content::Tint(..) => Canvas::new(cw, ch),
            Content::Edge(side, mode, phase) => edge_canvas(*side, t.frame(*mode), t.rim, cw, ch, u, *phase),
            Content::Ripple(ms) => {
                let mut c = Canvas::new(cw, ch);
                let (cx, cy) = (cw as f32 / 2.0, ch as f32 / 2.0);
                let s = *ms as f32 / 1000.0;
                // The splash: a drop that lands and is swallowed.
                if s < 0.16 {
                    let k = 1.0 - s / 0.16;
                    c.disc(cx, cy, (5.0 + 3.0 * k) * u, t.click.with_alpha(k));
                }
                for i in 0..RINGS {
                    let age = s - i as f32 * RING_GAP;
                    if !(0.0..RING_LIFE).contains(&age) {
                        continue;
                    }
                    let k = age / RING_LIFE;
                    let ease = 1.0 - (1.0 - k).powi(3); // fast out, slow to settle, like water
                    let radius = (6.0 + (RIPPLE_RADIUS - 6.0) * ease) * u;
                    let width = ((4.0 - 2.5 * k) * u).max(1.0);
                    let fade = (1.0 - k).powf(1.4) * (1.0 - 0.25 * i as f32);
                    c.ring(cx, cy, radius, width, t.click.with_alpha(fade));
                }
                c
            }
            Content::Tail(a, b, ms) => {
                let mut c = Canvas::new(cw, ch);
                let s = *ms as f32 / 1000.0;
                let head = (s / TAIL_DRAW).min(1.0);
                let head = 1.0 - (1.0 - head).powi(2);
                let end = ((s - TAIL_DRAW * 0.5) / TAIL_FADE).clamp(0.0, 1.0);
                if end >= head {
                    return c;
                }
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let len = dx.hypot(dy).max(1.0);
                let samples = (len * (head - end) / 2.0).ceil().max(2.0) as u32;
                for k in 0..=samples {
                    let f = end + (head - end) * k as f32 / samples as f32;
                    let rel = (f - end) / (head - end); // 0 at the fading end, 1 at the head
                    let (x, y) = ((a.0 + dx * f) * u, (a.1 + dy * f) * u);
                    c.disc(x, y, (1.0 + 4.0 * rel) * u, t.trail.with_alpha(rel.powf(1.2)));
                }
                let (hx, hy) = ((a.0 + dx * head) * u, (a.1 + dy * head) * u);
                c.disc(hx, hy, 5.5 * u, t.trail);
                c
            }
            Content::Caption(text, mode) => {
                let mut c = Canvas::new(cw, ch);
                let Some(font) = &self.font else { return c };
                let accent = if *mode == Mode::Driving { t.caption_border } else { t.stopped[0] };
                let b = (CAPTION_BORDER * u).max(1.0);
                let radius = if t.pixel > 1 { 0.0 } else { 10.0 * u };
                c.rounded_rect(0.0, 0.0, cw as f32, ch as f32, radius, accent);
                c.rounded_rect(b, b, cw as f32 - 2.0 * b, ch as f32 - 2.0 * b, (radius - b).max(0.0), t.caption_bg);
                let px = t.caption_px * u;
                let (asc, desc) = font.horizontal_line_metrics(px).map_or((px * 0.8, -px * 0.2), |m| (m.ascent, m.descent));
                let baseline = (ch as f32 + asc + desc) / 2.0;
                let fg = if *mode == Mode::Driving { t.caption_fg } else { t.stopped[1] };
                c.text(font, px, CAPTION_PAD * u, baseline, text, fg);
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
            let level = ((t / PULSE.as_secs_f32() * std::f32::consts::TAU).sin() * 0.5 + 0.5) * 255.0;
            let wash = if self.tint { Content::Tint(self.mode, level.round() as u8) } else { Content::Clear };
            self.set(TINT_L, wash);
            if self.role == Role::Driven {
                let step = self.theme.pixel.max(1);
                let phase = ((t * STRIPE_PX_PER_S) as u32 / step * step) % STRIPE as u32;
                for (i, side) in EDGES {
                    self.set(i, Content::Edge(side, self.mode, phase));
                }
            }
        }
        if self.role != Role::Driven {
            return;
        }
        if now.duration_since(self.motion_at) >= MOTION_STEP {
            self.motion_at = now;
            let quantise = |since: Instant| (now.duration_since(since).as_millis() as u32) / 33 * 33;
            if let Some(start) = self.ripple_since {
                let ms = quantise(start);
                let total = ((RING_GAP * (RINGS - 1) as f32 + RING_LIFE) * 1000.0) as u32;
                if ms >= total {
                    self.ripple_since = None;
                    self.set(RIPPLE_L, Content::Clear);
                } else {
                    self.set(RIPPLE_L, Content::Ripple(ms));
                }
            }
            if let Some((a, b, start)) = self.tail {
                let ms = quantise(start);
                if ms as f32 >= (TAIL_DRAW * 0.5 + TAIL_FADE) * 1000.0 {
                    self.tail = None;
                    self.set(TAIL_L, Content::Clear);
                } else {
                    self.set(TAIL_L, Content::Tail(a, b, ms));
                }
            }
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
                let half = (RIPPLE / 2) as i32;
                self.place(RIPPLE_L, x.round() as i32 - half, y.round() as i32 - half);
                self.ripple_since = Some(Instant::now());
                self.set(RIPPLE_L, Content::Ripple(0));
            }
            Cmd::Jump(a, b) if driven => {
                let (x0, y0) = ((a.0.min(b.0) - f64::from(TAIL_PAD)).round(), (a.1.min(b.1) - f64::from(TAIL_PAD)).round());
                let (w, h) = ((a.0 - b.0).abs() + 2.0 * f64::from(TAIL_PAD), (a.1 - b.1).abs() + 2.0 * f64::from(TAIL_PAD));
                if (a.0 - b.0).abs().max((a.1 - b.1).abs()) > 24.0 {
                    self.place(TAIL_L, x0 as i32, y0 as i32);
                    self.resize(TAIL_L, w.ceil() as u32, h.ceil() as u32);
                    let local = |p: (f64, f64)| ((p.0 - x0) as f32, (p.1 - y0) as f32);
                    self.tail = Some((local(a), local(b), Instant::now()));
                    self.set(TAIL_L, Content::Tail(local(a), local(b), 0));
                }
            }
            Cmd::Caption(text) if driven => {
                if let Some(font) = &self.font {
                    let px = self.theme.caption_px;
                    let max = (self.monitor.width as f32 - 80.0).max(120.0);
                    let text = draw::fit(font, px, &text, max - 2.0 * CAPTION_PAD);
                    let w = draw::text_width(font, px, &text) + 2.0 * CAPTION_PAD;
                    let h = px + 2.0 * CAPTION_PAD;
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
        let Some(l) = self.layers.iter().find(|l| l.size.is_some()) else { return Ok(()) };
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
            let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
            let ts = rustix::event::Timespec { tv_sec: 0, tv_nsec: timeout.as_nanos() as i64 };
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

/// One frame edge on a `cw` x `ch` canvas (`u` canvas px per logical px): the theme's rim at the
/// screen's edge and on the inside (if it has one), and between them a band of diagonal stripes
/// in its two colours, offset by `phase` logical px so they march.
fn edge_canvas(side: Side, colours: [Rgba; 2], rim: Option<Rgba>, cw: u32, ch: u32, u: f32, phase: u32) -> Canvas {
    let mut c = Canvas::new(cw, ch);
    let depth = match side {
        Side::Top | Side::Bottom => ch,
        Side::Left | Side::Right => cw,
    };
    let rim_px = rim.map_or(0, |_| ((EDGE_RIM * u).round() as u32).min(depth / 4));
    let period = ((STRIPE * u).round() as u32).max(2);
    let shift = (phase as f32 * u).round() as u32;
    let (a, b) = (colours[0].premultiplied(), colours[1].premultiplied());
    let r = rim.map_or(0, Rgba::premultiplied);
    for y in 0..ch {
        for x in 0..cw {
            // `d`: in from the screen's edge; `along`: along the edge. Both in canvas px.
            let (d, along) = match side {
                Side::Top => (y, x),
                Side::Bottom => (ch - 1 - y, x),
                Side::Left => (x, y),
                Side::Right => (cw - 1 - x, y),
            };
            c.px[(y * cw + x) as usize] = if d < rim_px || d + rim_px >= depth {
                r
            } else if (along + d + shift) % period < period / 2 {
                a
            } else {
                b
            };
        }
    }
    c
}

/// 4x4 ordered dither on alpha: each pixel is kept fully opaque (its colour un-premultiplied) or
/// dropped, so a fade becomes a thinning dot pattern instead of a blend, as on 8-bit hardware.
fn dither(mut c: Canvas) -> Canvas {
    const BAYER: [[u32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    for y in 0..c.h {
        for x in 0..c.w {
            let i = (y * c.w + x) as usize;
            let px = c.px[i];
            let a = px >> 24;
            if a == 0 {
                continue;
            }
            let threshold = (BAYER[(y % 4) as usize][(x % 4) as usize] * 2 + 1) * 255 / 32;
            c.px[i] = if a > threshold {
                let un = |sh: u32| (((px >> sh) & 0xff) * 255 / a).min(255) << sh;
                0xff00_0000 | un(16) | un(8) | un(0)
            } else {
                0
            };
        }
    }
    c
}

/// Scale `c` up by `k` whole pixels into a `w` x `h` canvas (cropping what spills over).
fn upscale(c: &Canvas, k: u32, w: u32, h: u32) -> Canvas {
    let mut out = Canvas::new(w, h);
    for y in 0..h {
        let sy = (y / k).min(c.h.saturating_sub(1));
        for x in 0..w {
            let sx = (x / k).min(c.w.saturating_sub(1));
            out.px[(y * w + x) as usize] = c.px[(sy * c.w + sx) as usize];
        }
    }
    out
}

/// The system's bold sans for captions, found through fontconfig; none means no captions.
fn caption_font() -> Option<fontdue::Font> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans-serif:bold"]).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    let bytes = std::fs::read(path.trim()).ok()?;
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
}

// ----- event plumbing -----------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event {
            state.outputs[*index].1 = Some(name);
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for State {
    fn event(state: &mut Self, _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => state.configures.push((*index, serial, width, height)),
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
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

    const Y: Rgba = Rgba(0xff, 0xc4, 0x00, 0xff);
    const K: Rgba = Rgba(0x14, 0x14, 0x14, 0xff);

    #[test]
    fn an_edge_is_two_tone_stripes_that_march_with_rims_when_the_theme_has_them() {
        let at = |c: &Canvas, x: u32, y: u32| c.px[(y * c.w + x) as usize];
        let c = edge_canvas(Side::Top, [Y, K], None, 64, EDGE, 1.0, 0);
        let band: std::collections::HashSet<u32> = (0..64).map(|x| at(&c, x, EDGE / 2)).collect();
        assert_eq!(band.len(), 2, "two-tone stripes");
        let moved = edge_canvas(Side::Top, [Y, K], None, 64, EDGE, 1.0, STRIPE as u32 / 2);
        assert_ne!(at(&c, 0, EDGE / 2), at(&moved, 0, EDGE / 2), "half a period later the stripe has swapped");
        let rim = Rgba(0, 0, 0, 0xc0);
        let r = edge_canvas(Side::Right, [Y, K], Some(rim), EDGE, 8, 1.0, 0);
        assert_eq!(at(&r, EDGE - 1, 3), rim.premultiplied(), "a right edge's outer rim is its last column");
        assert_eq!(at(&r, 0, 3), rim.premultiplied(), "and its inner rim the first");
    }

    #[test]
    fn dithering_keeps_pixels_whole_and_thins_a_fade_into_a_pattern() {
        let mut c = Canvas::new(8, 8);
        for (i, px) in c.px.iter_mut().enumerate() {
            *px = Y.with_alpha(if i < 32 { 1.0 } else { 0.5 }).premultiplied();
        }
        let d = dither(c);
        assert!(d.px.iter().all(|&p| p == 0 || p >> 24 == 0xff), "every pixel is whole or gone");
        assert!(d.px[..32].iter().all(|&p| p == Y.premultiplied()), "opaque stays, colour intact");
        let half = d.px[32..].iter().filter(|&&p| p != 0).count();
        assert!((12..=20).contains(&half), "a half fade keeps about half the dots: {half}");
    }

    #[test]
    fn upscaling_is_by_whole_pixels_and_crops_to_the_layer() {
        let mut c = Canvas::new(2, 1);
        c.px = vec![1, 2];
        let u = upscale(&c, 3, 5, 2);
        assert_eq!(u.px, vec![1, 1, 1, 2, 2, 1, 1, 1, 2, 2]);
    }
}
