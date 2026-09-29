//! One long-lived Wayland connection: screencopy captures, a virtual pointer, a virtual keyboard.
//!
//! Captures go through `zwlr_screencopy_manager_v1` into a shared-memory buffer that is kept and
//! reused while the size and format stay the same, so a capture is one compositor copy plus our
//! conversion. The pointer only ever sends buttons and scroll; positioning goes through
//! Hyprland's own `movecursor` dispatcher (see server.rs), which is exact on any monitor layout,
//! the split hypruse (MIT, Ilyas Khallouki) settled on after absolute virtual-pointer motion
//! misbehaved on multi-monitor setups (hyprwm/Hyprland#6749).

use crate::keymap::{self, Combo, Keymap};
use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::os::fd::{AsFd, OwnedFd};
use std::time::Instant;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
/// Continuous-axis length of one wheel notch (hypruse's value).
const SCROLL_UNITS_PER_NOTCH: f64 = 15.0;
const XKB_V1: u32 = 1;

struct Output {
    output: wl_output::WlOutput,
    name: Option<String>,
}

#[derive(Default)]
struct FrameState {
    buffer: Option<(WEnum<wl_shm::Format>, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    frame: FrameState,
}

/// A shared-memory buffer kept across captures.
struct Shm {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    key: (u32, u32, u32, u32), // format, width, height, stride
}

impl Drop for Shm {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: ptr/len came from the mmap in `Wl::shm_for` and nothing else holds them.
        unsafe {
            let _ = munmap(self.ptr.cast(), self.len);
        }
        let _ = &self.fd;
    }
}

/// A captured frame, already in the buffer; `rgb()` and `qoi()` convert it.
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
    bytes: &'a [u8],
}

impl Frame<'_> {
    /// Packed RGB rows, top to bottom.
    pub fn rgb(&self) -> Result<Vec<u8>> {
        // Byte order in memory of the little-endian 32-bit formats.
        let (r, g, b) = match self.format {
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (2, 1, 0),
            wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => (0, 1, 2),
            other => bail!("unsupported capture format {other:?}"),
        };
        let (w, h, stride) = (self.width as usize, self.height as usize, self.stride as usize);
        let mut out = vec![0u8; w * h * 3];
        for (row, dst) in out.chunks_exact_mut(w * 3).enumerate() {
            let src_row = if self.y_invert { h - 1 - row } else { row };
            let src = &self.bytes[src_row * stride..src_row * stride + w * 4];
            for (px, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
                d[0] = px[r];
                d[1] = px[g];
                d[2] = px[b];
            }
        }
        Ok(out)
    }

    /// Lossless QOI: an order of magnitude faster to encode than PNG, and Pillow reads it.
    pub fn qoi(&self) -> Result<Vec<u8>> {
        let rgb = self.rgb()?;
        qoi::encode_to_vec(&rgb, self.width, self.height).map_err(|e| anyhow!("qoi: {e}"))
    }
}

pub struct Wl {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    shm: wl_shm::WlShm,
    screencopy: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    keyboard: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    pointer_version: u32,
    keymap_loaded: Option<String>,
    buf: Option<Shm>,
    epoch: Instant,
}

impl Wl {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("reading Wayland globals")?;
        let qh = queue.handle();
        let mut state = State::default();

        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("wl_shm")?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).context("wl_seat")?;
        let screencopy: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 =
            globals.bind(&qh, 1..=3, ()).context("the compositor lacks zwlr_screencopy_manager_v1")?;
        let vpm: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 1..=2, ()).context("the compositor lacks zwlr_virtual_pointer_manager_v1")?;
        let vkm: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).context("the compositor lacks zwp_virtual_keyboard_manager_v1")?;

        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                let index = state.outputs.len();
                let output: wl_output::WlOutput =
                    globals.registry().bind(global.name, global.version.min(4), &qh, index);
                state.outputs.push(Output { output, name: None });
            }
        }

        let pointer_version = vpm.version();
        let pointer = vpm.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = vkm.create_virtual_keyboard(&seat, &qh, ());
        queue.roundtrip(&mut state).context("Wayland roundtrip")?; // output names arrive here

        Ok(Self {
            conn,
            queue,
            qh,
            state,
            shm,
            screencopy,
            pointer,
            keyboard,
            pointer_version,
            keymap_loaded: None,
            buf: None,
            epoch: Instant::now(),
        })
    }

    pub fn output_names(&self) -> Vec<String> {
        self.state.outputs.iter().filter_map(|o| o.name.clone()).collect()
    }

    fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn roundtrip(&mut self) -> Result<()> {
        self.queue.roundtrip(&mut self.state).context("Wayland roundtrip")?;
        Ok(())
    }

    // ----- capture ------------------------------------------------------------------------

    /// Capture one output, or a region of it in the output's logical coordinates.
    pub fn capture(&mut self, output: &str, region: Option<(i32, i32, i32, i32)>) -> Result<Frame<'_>> {
        let out = self
            .state
            .outputs
            .iter()
            .find(|o| o.name.as_deref() == Some(output))
            .ok_or_else(|| anyhow!("no Wayland output named {output}; have {:?}", self.output_names()))?
            .output
            .clone();
        self.state.frame = FrameState::default();
        let frame = match region {
            Some((x, y, w, h)) => self.screencopy.capture_output_region(0, &out, x, y, w, h, &self.qh, ()),
            None => self.screencopy.capture_output(0, &out, &self.qh, ()),
        };
        // v3 announces every buffer type then buffer_done; older versions stop after `buffer`.
        let v3 = self.screencopy.version() >= 3;
        while !(self.state.frame.failed
            || self.state.frame.buffer.is_some() && (!v3 || self.state.frame.buffer_done))
        {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        if self.state.frame.failed {
            frame.destroy();
            bail!("the compositor refused to capture {output}");
        }
        let (format, width, height, stride) = self.state.frame.buffer.ok_or_else(|| anyhow!("no shm buffer offered"))?;
        let format = match format {
            WEnum::Value(f) => f,
            WEnum::Unknown(u) => bail!("unknown shm format {u:#x}"),
        };
        self.shm_for(format, width, height, stride)?;
        let buffer = &self.buf.as_ref().unwrap().buffer;
        frame.copy(buffer);
        while !(self.state.frame.ready || self.state.frame.failed) {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        frame.destroy();
        if self.state.frame.failed {
            bail!("the copy of {output} failed");
        }
        let shm = self.buf.as_ref().unwrap();
        // SAFETY: the compositor finished writing (`ready`); the mapping lives as long as `self.buf`,
        // and the returned frame borrows `self` mutably, so no new capture can reuse it meanwhile.
        let bytes = unsafe { std::slice::from_raw_parts(shm.ptr, (stride * height) as usize) };
        Ok(Frame { width, height, stride, format, y_invert: self.state.frame.y_invert, bytes })
    }

    fn shm_for(&mut self, format: wl_shm::Format, width: u32, height: u32, stride: u32) -> Result<()> {
        let key = (format as u32, width, height, stride);
        if self.buf.as_ref().is_some_and(|b| b.key == key) {
            return Ok(());
        }
        self.buf = None;
        let len = (stride * height) as usize;
        let fd = memfd_create("hyprhands-capture", MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, len as u64)?;
        // SAFETY: a fresh shared mapping of our own memfd, unmapped in Shm's Drop.
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, MapFlags::SHARED, &fd, 0)? };
        let pool = self.shm.create_pool(fd.as_fd(), len as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &self.qh, ());
        self.buf = Some(Shm { fd, ptr: ptr.cast(), len, pool, buffer, key });
        Ok(())
    }

    // ----- pointer ------------------------------------------------------------------------

    pub fn button(&mut self, name: &str, pressed: bool) -> Result<()> {
        let code = match name {
            "left" => BTN_LEFT,
            "right" => BTN_RIGHT,
            "middle" => BTN_MIDDLE,
            other => bail!("unknown button {other:?}"),
        };
        let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        self.pointer.button(self.now_ms(), code, state);
        self.pointer.frame();
        self.roundtrip()
    }

    /// Press and release, with the short hold a physical click has; the release happens even if
    /// the press errored, so a button is never left down.
    pub fn click(&mut self, name: &str) -> Result<()> {
        let pressed = self.button(name, true);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let released = self.button(name, false);
        pressed.and(released)
    }

    /// Wheel notches under the cursor; positive scrolls down.
    pub fn scroll(&mut self, notches: i32) -> Result<()> {
        if notches == 0 {
            return Ok(());
        }
        let t = self.now_ms();
        let value = f64::from(notches) * SCROLL_UNITS_PER_NOTCH;
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        if self.pointer_version >= 2 {
            self.pointer.axis_discrete(t, wl_pointer::Axis::VerticalScroll, value, notches);
        } else {
            self.pointer.axis(t, wl_pointer::Axis::VerticalScroll, value);
        }
        self.pointer.frame();
        self.roundtrip()
    }

    // ----- keyboard -----------------------------------------------------------------------

    fn load_keymap(&mut self, km: &Keymap) -> Result<()> {
        if self.keymap_loaded.as_deref() == Some(km.text.as_str()) {
            return Ok(());
        }
        let mut bytes = km.text.clone().into_bytes();
        bytes.push(0);
        let fd = memfd_create("hyprhands-keymap", MemfdFlags::CLOEXEC)?;
        rustix::io::write(&fd, &bytes)?;
        self.keyboard.keymap(XKB_V1, fd.as_fd(), bytes.len() as u32);
        self.roundtrip()?;
        self.keymap_loaded = Some(km.text.clone());
        Ok(())
    }

    fn key(&mut self, code: u32, pressed: bool) {
        self.keyboard.key(self.now_ms(), code, u32::from(pressed));
    }

    /// Type literal text, any characters, whatever the owner's layout.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let (km, chars) = keymap::for_text(text);
        self.load_keymap(&km)?;
        for ch in text.chars() {
            let index = chars.iter().position(|c| *c == ch).unwrap();
            let code = km.codes[&format!("C{index}")];
            self.key(code, true);
            self.key(code, false);
            self.conn.flush()?;
        }
        self.roundtrip()
    }

    /// Press a combo: modifiers down in order, the key, then everything up in reverse.
    pub fn combo(&mut self, combo: &Combo) -> Result<()> {
        let km = keymap::for_combo(combo);
        self.load_keymap(&km)?;
        let order: Vec<u32> = combo
            .mods
            .iter()
            .map(|m| km.codes[&format!("M_{m}")])
            .chain(std::iter::once(km.codes["KEY"]))
            .collect();
        for code in &order {
            self.key(*code, true);
        }
        for code in order.iter().rev() {
            self.key(*code, false);
        }
        self.roundtrip()
    }
}

// ----- event plumbing ----------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event {
            state.outputs[*index].name = Some(name);
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let f = &mut state.frame;
        match event {
            Event::Buffer { format, width, height, stride } => f.buffer = Some((format, width, height, stride)),
            Event::BufferDone => f.buffer_done = true,
            Event::Flags { flags } => {
                f.y_invert = matches!(flags, WEnum::Value(v) if v.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            Event::Ready { .. } => f.ready = true,
            Event::Failed => f.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(State: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(State: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bytes: &[u8], format: wl_shm::Format, y_invert: bool) -> Frame<'_> {
        Frame { width: 2, height: 2, stride: 8, format, y_invert, bytes }
    }

    // two rows of two pixels, XRGB little-endian in memory: B G R X
    const XRGB: [u8; 16] = [3, 2, 1, 0, 6, 5, 4, 0, 9, 8, 7, 0, 12, 11, 10, 0];

    #[test]
    fn xrgb_rows_become_packed_rgb() {
        let rgb = frame(&XRGB, wl_shm::Format::Xrgb8888, false).rgb().unwrap();
        assert_eq!(rgb, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn y_inverted_frames_come_out_upright() {
        let rgb = frame(&XRGB, wl_shm::Format::Xrgb8888, true).rgb().unwrap();
        assert_eq!(&rgb[..6], &[7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn qoi_round_trips() {
        let f = frame(&XRGB, wl_shm::Format::Xrgb8888, false);
        let encoded = f.qoi().unwrap();
        let (header, pixels) = qoi::decode_to_vec(&encoded).unwrap();
        assert_eq!((header.width, header.height), (2, 2));
        assert_eq!(pixels, f.rgb().unwrap());
    }
}
