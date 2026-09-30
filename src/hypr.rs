//! Hyprland's IPC, spoken directly: the request socket (`.socket.sock`) and the event stream
//! (`.socket2.sock`). No `hyprctl` process per call, so polling the cursor every 100 ms is cheap.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where one Hyprland instance's sockets live, and which dispatcher language it speaks: with a
/// Lua config (0.55+), `dispatch` evaluates `hl.dispatch(<args>)`, so the legacy
/// `movecursor 10 20` form is a Lua syntax error there.
#[derive(Clone, Debug)]
pub struct Instance {
    dir: PathBuf,
    lua: bool,
}

impl Instance {
    /// The instance named by HYPRLAND_INSTANCE_SIGNATURE, or the most recently started one under
    /// $XDG_RUNTIME_DIR/hypr (what an ssh session without the variable needs).
    pub fn discover() -> Result<Self> {
        let runtime = runtime_dir();
        let hypr = runtime.join("hypr");
        if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
            if !sig.is_empty() {
                return Ok(Self::at(hypr.join(sig)));
            }
        }
        let newest = std::fs::read_dir(&hypr)
            .with_context(|| format!("no Hyprland instances under {}", hypr.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(".socket.sock").exists())
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            .ok_or_else(|| anyhow!("no running Hyprland instance under {}", hypr.display()))?;
        Ok(Self::at(newest.path()))
    }

    /// `hl.dsp.no_op()` is a harmless dispatch under a Lua config and an unknown dispatcher under a
    /// hyprlang one.
    fn at(dir: PathBuf) -> Self {
        let mut i = Self { dir, lua: false };
        i.lua = i.request_raw("dispatch hl.dsp.no_op()").is_ok_and(|r| r.trim() == "ok");
        i
    }

    pub fn lua(&self) -> bool {
        self.lua
    }

    pub fn signature(&self) -> String {
        self.dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// One raw request; the reply is whatever Hyprland writes before closing the socket.
    pub fn request_raw(&self, command: &str) -> Result<String> {
        let mut s = UnixStream::connect(self.dir.join(".socket.sock")).context("Hyprland request socket")?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(command.as_bytes())?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out)
    }

    /// A JSON query: `j/<command>`.
    pub fn query(&self, command: &str) -> Result<Value> {
        let raw = self.request_raw(&format!("j/{command}"))?;
        serde_json::from_str(&raw).with_context(|| format!("Hyprland answered {command} with non-JSON: {raw:.200}"))
    }

    /// Warp the cursor to a global logical point.
    pub fn move_cursor(&self, x: i64, y: i64) -> Result<()> {
        self.dispatch(&if self.lua { format!("hl.dsp.cursor.move({{ x = {x}, y = {y} }})") } else { format!("movecursor {x} {y}") })
    }

    pub fn focus_window(&self, address: &str) -> Result<()> {
        self.dispatch(&if self.lua {
            format!("hl.dsp.focus({{ window = {} }})", lua_str(&format!("address:{address}")))
        } else {
            format!("focuswindow address:{address}")
        })
    }

    /// Move a window to a workspace without following it there.
    pub fn move_window_silent(&self, address: &str, workspace: i64) -> Result<()> {
        self.dispatch(&if self.lua {
            format!(
                "hl.dsp.window.move({{ workspace = {}, follow = false, window = {} }})",
                lua_str(&workspace.to_string()),
                lua_str(&format!("address:{address}"))
            )
        } else {
            format!("movetoworkspacesilent {workspace},address:{address}")
        })
    }

    /// Run a shell command whose windows open on `workspace` without taking focus there.
    pub fn exec_on_workspace(&self, cmd: &str, workspace: i64) -> Result<()> {
        self.dispatch(&if self.lua {
            format!("hl.dsp.exec_cmd({}, {{ workspace = {} }})", lua_str(cmd), lua_str(&format!("{workspace} silent")))
        } else {
            format!("exec [workspace {workspace} silent] {cmd}")
        })
    }

    /// A dispatcher, the way `hyprctl dispatch <args>` runs one, in this instance's dialect.
    pub fn dispatch(&self, args: &str) -> Result<()> {
        let reply = self.request_raw(&format!("dispatch {args}"))?;
        if reply.trim() != "ok" {
            bail!("dispatch {args}: {}", reply.trim());
        }
        Ok(())
    }

    pub fn cursor(&self) -> Result<(f64, f64)> {
        let v = self.query("cursorpos")?;
        Ok((num(&v["x"])?, num(&v["y"])?))
    }

    /// Subscribe to the event stream. One reader thread per call; every event line arrives on the
    /// channel as (name, data), split at the first `>>`.
    pub fn events(&self) -> Result<Receiver<(String, String)>> {
        let stream = UnixStream::connect(self.dir.join(".socket2.sock")).context("Hyprland event socket")?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let (name, data) = line.split_once(">>").unwrap_or((line.as_str(), ""));
                if tx.send((name.to_owned(), data.to_owned())).is_err() {
                    break;
                }
            }
        });
        Ok(rx)
    }
}

/// Fans one event stream out to any number of listeners, so the takeover watcher and a `launch`
/// waiting for its window share one socket.
#[derive(Clone, Default)]
pub struct EventHub {
    listeners: Arc<Mutex<Vec<Sender<(String, String)>>>>,
}

impl EventHub {
    pub fn start(instance: &Instance) -> Result<Self> {
        let hub = Self::default();
        let rx = instance.events()?;
        let listeners = hub.listeners.clone();
        std::thread::spawn(move || {
            for event in rx {
                listeners.lock().unwrap().retain(|l| l.send(event.clone()).is_ok());
            }
        });
        Ok(hub)
    }

    pub fn subscribe(&self) -> Receiver<(String, String)> {
        let (tx, rx) = channel();
        self.listeners.lock().unwrap().push(tx);
        rx
    }
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())))
}

pub fn num(v: &Value) -> Result<f64> {
    v.as_f64().ok_or_else(|| anyhow!("expected a number, got {v}"))
}

/// A monitor as Hyprland reports it, reduced to what placing input and captures needs.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Monitor {
    pub name: String,
    pub id: i64,
    pub x: f64,
    pub y: f64,
    /// Logical size: pixels over scale, and swapped when the output is rotated a quarter turn.
    pub width: f64,
    pub height: f64,
    pub scale: f64,
    pub workspace: i64,
    pub focused: bool,
}

impl Monitor {
    pub fn from_json(m: &Value) -> Result<Self> {
        let scale = num(&m["scale"])?;
        let (mut w, mut h) = (num(&m["width"])? / scale, num(&m["height"])? / scale);
        if m["transform"].as_i64().unwrap_or(0) % 2 == 1 {
            std::mem::swap(&mut w, &mut h);
        }
        Ok(Self {
            name: m["name"].as_str().unwrap_or_default().to_owned(),
            id: m["id"].as_i64().unwrap_or(-1),
            x: num(&m["x"])?,
            y: num(&m["y"])?,
            width: w,
            height: h,
            scale,
            workspace: m["activeWorkspace"]["id"].as_i64().unwrap_or(0),
            focused: m["focused"].as_bool().unwrap_or(false),
        })
    }
}

/// A Lua string literal holding exactly `s`.
fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            // Other control characters as decimal byte escapes; everything else is UTF-8 as is.
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn monitors(instance: &Instance) -> Result<Vec<Monitor>> {
    instance.query("monitors")?.as_array().ok_or_else(|| anyhow!("monitors: not a list"))?.iter().map(Monitor::from_json).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lua_strings_escape_quotes_backslashes_and_controls() {
        assert_eq!(lua_str(r#"it's "q" \ ok"#), r#""it's \"q\" \\ ok""#);
        assert_eq!(lua_str("a\nb\tc Wörld ✓"), "\"a\\nb\\009c Wörld ✓\"");
    }

    #[test]
    fn a_rotated_scaled_monitor_reports_its_logical_box() {
        let m = Monitor::from_json(&json!({
            "name": "DP-1", "id": 1, "x": 3440, "y": -560, "width": 2560, "height": 1440,
            "scale": 1.5, "transform": 3, "activeWorkspace": {"id": 2}, "focused": false
        }))
        .unwrap();
        assert_eq!((m.width.round(), m.height.round()), (960.0, 1707.0));
        assert_eq!(m.workspace, 2);
    }
}
