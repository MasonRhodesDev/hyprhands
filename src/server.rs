//! The session: one driven monitor, the ops an agent loop calls, and the takeover watch.
//!
//! Every coordinate in and out is in the driven monitor's logical space with its top-left at
//! 0,0 (the one display an agent like jev believes it has); the server adds the monitor's origin
//! before touching the compositor.
//!
//! The server never writes the owner's config: it reads binds and options (config.rs) and uses
//! only dispatchers that act on windows and the cursor.

use crate::a11y::{self, A11y};
use crate::config::Config;
use crate::hypr::{self, EventHub, Instance, Monitor};
use crate::keymap;
use crate::screentext;
use crate::takeover::Takeover;
use crate::wl::Wl;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_TOLERANCE: f64 = 64.0;
const CURSOR_POLL: Duration = Duration::from_millis(100);
const LAUNCH_WAIT: Duration = Duration::from_secs(10);
const CLICK_SETTLE: Duration = Duration::from_millis(20);
/// Ops that put input on the seat: refused once the owner took over or pressed the panic file.
const INPUT_OPS: [&str; 9] = ["click", "move", "scroll", "key", "type", "focus", "bring", "launch", "spawn"];

pub fn stop_file() -> PathBuf {
    hypr::runtime_dir().join("hyprhands-stop")
}

pub struct Server {
    hypr: Instance,
    wl: Wl,
    hub: EventHub,
    monitor: Monitor,
    config: Config,
    takeover: Arc<Mutex<Takeover>>,
    /// Connected on the first tree request, and again after a failure: the bus may come up later.
    a11y: Option<A11y>,
    /// The window the agent launched, focused or brought, while it stays on the driven workspace:
    /// what it chose to work in, whether its own window or one of the owner's. Without one, the
    /// agent's window is the driven workspace's most recently focused one.
    claimed: Option<String>,
}

impl Server {
    pub fn start(monitor: Option<&str>, tolerance: f64) -> Result<Self> {
        let hypr = Instance::discover()?;
        let wl = Wl::connect()?;
        let hub = EventHub::start(&hypr)?;
        let monitors = hypr::monitors(&hypr)?;
        let monitor = match monitor {
            Some(name) => monitors.iter().find(|m| m.name == name),
            None => monitors.iter().find(|m| m.focused).or(monitors.first()),
        }
        .cloned()
        .ok_or_else(|| anyhow!("no monitor {monitor:?}; have {:?}", monitors.iter().map(|m| &m.name).collect::<Vec<_>>()))?;
        let config = Config::load(&hypr)?;
        let _ = std::fs::remove_file(stop_file()); // a press from an earlier session does not stop this one
        let mut t = Takeover::new(tolerance);
        t.baseline(hypr.cursor()?);
        let takeover = Arc::new(Mutex::new(t));
        let server = Self { hypr, wl, hub, monitor, config, takeover, a11y: None, claimed: None };
        server.watch();
        Ok(server)
    }

    /// Two watchers feed the takeover state between requests: the cursor, polled over the request
    /// socket, and the focused monitor, from the event stream.
    fn watch(&self) {
        let (hypr, seat) = (self.hypr.clone(), self.takeover.clone());
        std::thread::spawn(move || loop {
            if let Ok(pos) = hypr.cursor() {
                seat.lock().unwrap().cursor_seen(pos, Instant::now());
            }
            std::thread::sleep(CURSOR_POLL);
        });
        let (events, seat, driven) = (self.hub.subscribe(), self.takeover.clone(), self.monitor.name.clone());
        std::thread::spawn(move || {
            for (name, data) in events {
                if name == "focusedmon" {
                    let focused = data.split(',').next().unwrap_or_default();
                    seat.lock().unwrap().focused_monitor(focused, &driven, Instant::now());
                }
            }
        });
    }

    fn to_global(&self, x: f64, y: f64) -> (i64, i64) {
        ((self.monitor.x + x).round() as i64, (self.monitor.y + y).round() as i64)
    }

    fn to_local(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (x - self.monitor.x, y - self.monitor.y)
    }

    fn refused(&self) -> Option<String> {
        if stop_file().exists() {
            return Some("stopped: the panic file exists".into());
        }
        self.takeover.lock().unwrap().reason().map(str::to_owned)
    }

    fn window(&self) -> Result<Option<Value>> {
        if let Some(addr) = &self.claimed {
            let ws = hypr::monitors(&self.hypr)?.into_iter().find(|m| m.name == self.monitor.name).map(|m| m.workspace);
            let clients = self.hypr.query("clients")?;
            if clients.as_array().into_iter().flatten().any(|c| c["address"] == addr.as_str() && c["workspace"]["id"].as_i64() == ws) {
                return self.client(addr);
            }
        }
        agent_window(&self.hypr, &self.monitor)
    }

    /// Any window by address, wherever it is, shaped like `window()`'s.
    fn client(&self, address: &str) -> Result<Option<Value>> {
        let clients = self.hypr.query("clients")?;
        Ok(clients.as_array().into_iter().flatten().find(|c| c["address"] == address).map(|c| {
            let (x, y) = self.to_local((c["at"][0].as_f64().unwrap_or(0.0), c["at"][1].as_f64().unwrap_or(0.0)));
            json!({
                "address": c["address"], "class": c["class"], "title": c["title"], "pid": c["pid"],
                "frame": [x, y, c["size"][0], c["size"][1]],
            })
        }))
    }

    pub fn handle(&mut self, req: &Value) -> Result<(Value, Option<Vec<u8>>)> {
        let op = req["op"].as_str().ok_or_else(|| anyhow!("request without an op"))?;
        if INPUT_OPS.contains(&op) {
            if let Some(reason) = self.refused() {
                bail!("refused: {reason}");
            }
            self.takeover.lock().unwrap().begin();
            let result = self.input(op, req);
            let cursor = self.hypr.cursor().ok();
            self.takeover.lock().unwrap().end(cursor, Instant::now());
            return result.map(|v| (v, None));
        }
        match op {
            "hello" => Ok((json!({"monitor": self.monitor, "outputs": self.wl.output_names(), "instance": self.hypr.signature()}), None)),
            "state" => Ok((
                json!({
                    "window": self.window()?,
                    "cursor": self.hypr.cursor().map(|c| self.to_local(c)).ok(),
                    "takeover": self.refused(),
                }),
                None,
            )),
            "screenshot" => self.screenshot(req),
            "tree" => self.tree(req),
            "config" => Ok((serde_json::to_value(&self.config)?, None)),
            "find_window" => {
                let class = req["class"].as_str().unwrap_or_default();
                let clients = self.hypr.query("clients")?;
                let found = clients.as_array().into_iter().flatten().find(|c| c["class"] == class);
                Ok((json!({"address": found.map(|c| c["address"].clone()), "workspace": found.map(|c| c["workspace"]["id"].clone())}), None))
            }
            other => bail!("unknown op {other:?}"),
        }
    }

    fn screenshot(&mut self, req: &Value) -> Result<(Value, Option<Vec<u8>>)> {
        let region = req.get("region").and_then(Value::as_array).map(|r| {
            let n = |i: usize| r.get(i).and_then(Value::as_f64).unwrap_or(0.0).round() as i32;
            (n(0), n(1), n(2), n(3))
        });
        let started = Instant::now();
        let name = self.monitor.name.clone();
        let logical_w = region.map_or(self.monitor.width, |r| f64::from(r.2));
        let frame = self.wl.capture(&name, region)?;
        let captured = started.elapsed();
        let (w, h) = (frame.width, frame.height);
        let qoi = frame.qoi()?;
        Ok((
            json!({
                "format": "qoi", "width": w, "height": h, "scale": f64::from(w) / logical_w,
                "ms": {"capture": captured.as_secs_f64() * 1e3, "encode": (started.elapsed() - captured).as_secs_f64() * 1e3},
            }),
            Some(qoi),
        ))
    }

    /// The agent window's (or `address`'s) accessibility tree as OSWorld XML, and with `"text": true`
    /// its screen text as lines in capture pixels (null when the tree is too thin or capped to stand
    /// in for OCR).
    fn tree(&mut self, req: &Value) -> Result<(Value, Option<Vec<u8>>)> {
        let started = Instant::now();
        let win = match req["address"].as_str() {
            Some(addr) => self.client(addr)?,
            None => self.window()?,
        };
        let empty = |window: Value, a11y: bool| {
            let ms = started.elapsed().as_secs_f64() * 1e3;
            json!({"xml": a11y::Tree::default_xml(), "nodes": 0, "capped": false, "no_app": false, "window": window,
                   "lines": null, "a11y": a11y, "ms": {"walk": 0.0, "total": ms}})
        };
        let Some(win) = win else {
            return Ok((empty(Value::Null, self.a11y.is_some()), None));
        };
        if self.a11y.is_none() {
            // No bus: an empty tree, and the client reads pixels, as doctor says.
            match A11y::connect() {
                Ok(a) => self.a11y = Some(a),
                Err(_) => return Ok((empty(win["address"].clone(), false), None)),
            }
        }
        let tree = match self.a11y.as_ref().map(|a| a.tree(&target(&win, &self.monitor))) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                self.a11y = None;
                return Err(e);
            }
            None => unreachable!(),
        };
        let walked = started.elapsed();
        let lines = (req["text"].as_bool().unwrap_or(false) && !tree.capped)
            .then(|| screentext::lines(&tree, self.monitor.scale, self.monitor.width, self.monitor.height))
            .filter(|l| l.len() >= screentext::MIN_LINES);
        Ok((
            json!({
                "xml": tree.xml(), "nodes": tree.count, "capped": tree.capped, "no_app": tree.no_app,
                "window": win["address"], "lines": lines, "a11y": true,
                "ms": {"walk": walked.as_secs_f64() * 1e3, "total": started.elapsed().as_secs_f64() * 1e3},
            }),
            None,
        ))
    }

    fn arg_f64(req: &Value, name: &str) -> Result<f64> {
        req[name].as_f64().ok_or_else(|| anyhow!("missing number {name:?}"))
    }

    fn move_to(&self, x: f64, y: f64) -> Result<()> {
        let (gx, gy) = self.to_global(x, y);
        self.hypr.move_cursor(gx, gy)
    }

    fn focus_agent_window(&self) -> Result<()> {
        if let Some(w) = self.window()? {
            if let Some(addr) = w["address"].as_str() {
                self.focus(addr)?;
            }
        }
        Ok(())
    }

    /// Focus a window unless it already has focus (a redundant focus still warps the cursor), and
    /// tell the takeover watch where the owner's config will warp the cursor, if it does.
    fn focus(&self, addr: &str) -> Result<()> {
        if self.hypr.query("activewindow").is_ok_and(|w| w["address"] == addr) {
            return Ok(());
        }
        self.hypr.focus_window(addr)?;
        if self.config.warps_on_focus() {
            let clients = self.hypr.query("clients")?;
            if let Some(c) = clients.as_array().into_iter().flatten().find(|c| c["address"] == addr) {
                let n = |v: &Value| v.as_f64().unwrap_or(0.0);
                let centre = ((n(&c["at"][0]) + n(&c["size"][0]) / 2.0).floor(), (n(&c["at"][1]) + n(&c["size"][1]) / 2.0).floor());
                self.takeover.lock().unwrap().expect(centre);
            }
        }
        Ok(())
    }

    fn input(&mut self, op: &str, req: &Value) -> Result<Value> {
        match op {
            "move" => self.move_to(Self::arg_f64(req, "x")?, Self::arg_f64(req, "y")?)?,
            "click" => {
                self.move_to(Self::arg_f64(req, "x")?, Self::arg_f64(req, "y")?)?;
                std::thread::sleep(CLICK_SETTLE);
                self.wl.click(req["button"].as_str().unwrap_or("left"))?;
            }
            "scroll" => {
                if let (Some(x), Some(y)) = (req["x"].as_f64(), req["y"].as_f64()) {
                    self.move_to(x, y)?;
                }
                self.wl.scroll(req["notches"].as_i64().unwrap_or(0) as i32)?;
            }
            "key" => {
                let combo = keymap::parse_combo(req["combo"].as_str().unwrap_or_default())?;
                if let Some(bind) = self.config.swallowed_by(&combo) {
                    if !req["allow_bind"].as_bool().unwrap_or(false) {
                        bail!(
                            "{} is the owner's compositor bind ({}{}); it would not reach the app. Pass allow_bind to trigger it.",
                            bind.label(),
                            bind.dispatcher,
                            if bind.description.is_empty() { String::new() } else { format!(": {}", bind.description) }
                        );
                    }
                }
                self.focus_agent_window()?;
                self.wl.combo(&combo)?;
            }
            "type" => {
                self.focus_agent_window()?;
                self.wl.type_text(req["text"].as_str().unwrap_or_default())?;
            }
            "focus" => {
                let addr = req["address"].as_str().unwrap_or_default();
                self.focus(addr)?;
                self.claimed = Some(addr.to_owned());
            }
            "bring" => {
                let addr = req["address"].as_str().unwrap_or_default();
                let ws = hypr::monitors(&self.hypr)?.into_iter().find(|m| m.name == self.monitor.name).map(|m| m.workspace);
                let clients = self.hypr.query("clients")?;
                let at = clients.as_array().into_iter().flatten().find(|c| c["address"] == addr).and_then(|c| c["workspace"]["id"].as_i64());
                if let (Some(ws), Some(at)) = (ws, at) {
                    if ws != at {
                        self.hypr.move_window_silent(addr, ws)?;
                    }
                }
                self.focus(addr)?;
                self.claimed = Some(addr.to_owned());
            }
            "launch" => return self.launch(req),
            "spawn" => {
                let argv = argv(req)?;
                use std::os::unix::process::CommandExt;
                std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .process_group(0)
                    .spawn()?;
            }
            _ => unreachable!(),
        }
        Ok(json!({}))
    }

    /// Start an app on the driven monitor's workspace and wait for its window to open.
    fn launch(&mut self, req: &Value) -> Result<Value> {
        let argv = argv(req)?;
        let ws = hypr::monitors(&self.hypr)?.into_iter().find(|m| m.name == self.monitor.name).map_or(1, |m| m.workspace);
        let events = self.hub.subscribe();
        let cmd = argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
        self.hypr.exec_on_workspace(&cmd, ws)?;
        let deadline = Instant::now() + LAUNCH_WAIT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok((name, data)) if name == "openwindow" => {
                    let address = format!("0x{}", data.split(',').next().unwrap_or_default());
                    self.claimed = Some(address.clone());
                    return Ok(json!({"address": address}));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(json!({"address": null, "note": "launched, but no window opened in time (a single-instance app may have reused one)"}))
    }
}

/// The agent's window: the most recently focused one on the driven monitor's current workspace,
/// not Hyprland's active window, which follows the owner around the other monitors. Its frame is in
/// the monitor's logical space.
pub fn agent_window(hypr: &Instance, monitor: &Monitor) -> Result<Option<Value>> {
    let ws = hypr::monitors(hypr)?.into_iter().find(|m| m.name == monitor.name).map(|m| m.workspace);
    let clients = hypr.query("clients")?;
    let here = clients
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["workspace"]["id"].as_i64() == ws && c["mapped"].as_bool().unwrap_or(true) && !c["hidden"].as_bool().unwrap_or(false))
        .min_by_key(|c| c["focusHistoryID"].as_i64().unwrap_or(i64::MAX));
    Ok(here.map(|c| {
        let (x, y) = (c["at"][0].as_f64().unwrap_or(0.0) - monitor.x, c["at"][1].as_f64().unwrap_or(0.0) - monitor.y);
        json!({
            "address": c["address"], "class": c["class"], "title": c["title"], "pid": c["pid"],
            "frame": [x, y, c["size"][0], c["size"][1]],
        })
    }))
}

/// What a tree walk needs to find a window's app and place its frames.
pub fn target<'a>(win: &'a Value, monitor: &Monitor) -> a11y::Target<'a> {
    let f = |i: usize| win["frame"][i].as_f64().unwrap_or(0.0);
    a11y::Target {
        pid: win["pid"].as_u64().unwrap_or(0) as u32,
        title: win["title"].as_str().unwrap_or_default(),
        class: win["class"].as_str().unwrap_or_default(),
        origin: (f(0), f(1)),
        width: f(2),
        scale: monitor.scale,
    }
}

fn argv(req: &Value) -> Result<Vec<String>> {
    let argv: Vec<String> = req["argv"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(expand_env)).collect();
    if argv.is_empty() {
        bail!("missing argv");
    }
    Ok(argv)
}

/// `$HOME` and `${VAR}` in arguments, so a client can name paths on this machine.
fn expand_env(arg: &str) -> String {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let (name, after) = if let Some(stripped) = rest.strip_prefix('{') {
            stripped.split_once('}').unwrap_or((stripped, ""))
        } else {
            let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        };
        out.push_str(&std::env::var(name).unwrap_or_default());
        rest = after;
    }
    out.push_str(rest);
    out
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_and_quoting_for_launch_commands() {
        // SAFETY: tests in this module do not read this variable concurrently.
        unsafe { std::env::set_var("HH_TEST", "/home/x") };
        assert_eq!(expand_env("--user-data-dir=$HH_TEST/.cache/b"), "--user-data-dir=/home/x/.cache/b");
        assert_eq!(expand_env("${HH_TEST}x"), "/home/xx");
        assert_eq!(shell_quote("--flag=a,b"), "--flag=a,b");
        assert_eq!(shell_quote("it's here"), r"'it'\''s here'");
    }
}
