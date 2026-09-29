//! hyprhands: fast, safe hands on a Hyprland desktop for agent loops.
//!
//!   hyprhands serve [--monitor NAME] [--tolerance PX]   the framed stdio protocol (proto.rs)
//!   hyprhands doctor                                    what this session can and cannot do
//!   hyprhands bench [--monitor NAME] [-n N]             read-only timings; sends no input

mod config;
mod hypr;
mod keymap;
mod proto;
mod server;
mod takeover;
mod wl;

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::time::Instant;

struct Args {
    command: String,
    monitor: Option<String>,
    tolerance: f64,
    n: usize,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let command = it.next().unwrap_or_else(|| "help".into());
    let mut args = Args { command, monitor: None, tolerance: server::DEFAULT_TOLERANCE, n: 10 };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--monitor" => args.monitor = it.next(),
            "--tolerance" => args.tolerance = it.next().context("--tolerance needs a value")?.parse()?,
            "-n" => args.n = it.next().context("-n needs a value")?.parse()?,
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(args)
}

/// An ssh session has no WAYLAND_DISPLAY; the compositor's socket is in the runtime dir.
fn discover_wayland() {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return;
    }
    let mut sockets: Vec<String> = std::fs::read_dir(hypr::runtime_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
        .collect();
    sockets.sort();
    if let Some(first) = sockets.first() {
        // SAFETY: single-threaded here, before any thread is spawned.
        unsafe { std::env::set_var("WAYLAND_DISPLAY", first) };
    }
}

fn serve(args: &Args) -> Result<()> {
    let mut server = server::Server::start(args.monitor.as_deref(), args.tolerance)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let (mut input, mut output) = (stdin.lock(), stdout.lock());
    while let Some(req) = proto::read_request(&mut input)? {
        match server.handle(&req) {
            Ok((mut reply, blob)) => {
                reply["ok"] = true.into();
                proto::write_reply(&mut output, reply, blob.as_deref())?;
            }
            Err(e) => proto::write_reply(&mut output, json!({"ok": false, "error": format!("{e:#}")}), None)?,
        }
    }
    Ok(())
}

fn doctor() -> Result<()> {
    let ok = |what: &str, detail: String| println!("[ok]   {what:<12} {detail}");
    let instance = hypr::Instance::discover()?;
    let sig = instance.signature();
    ok("hyprland", format!("instance {}", &sig[..12.min(sig.len())]));
    for m in hypr::monitors(&instance)? {
        ok("monitor", format!("{} at {},{} {}x{} scale {} ws {}", m.name, m.x, m.y, m.width, m.height, m.scale, m.workspace));
    }
    let cfg = config::Config::load(&instance)?;
    ok("config", format!("{} binds, {} keyboards, read only", cfg.binds.len(), cfg.keyboards.len()));
    let wl = wl::Wl::connect()?;
    ok("wayland", format!("screencopy, virtual pointer and keyboard bound; outputs {:?}", wl.output_names()));
    Ok(())
}

fn bench(args: &Args) -> Result<()> {
    let instance = hypr::Instance::discover()?;
    let monitors = hypr::monitors(&instance)?;
    let monitor = match &args.monitor {
        Some(n) => monitors.iter().find(|m| &m.name == n).context("no such monitor")?,
        None => monitors.iter().find(|m| m.focused).unwrap_or(&monitors[0]),
    };
    let mut wl = wl::Wl::connect()?;
    let report = |label: &str, samples: &mut Vec<f64>| {
        samples.sort_by(f64::total_cmp);
        let p50 = samples[samples.len() / 2];
        let max = samples.last().copied().unwrap_or(0.0);
        println!("{label:<18} p50 {p50:8.2} ms   max {max:8.2} ms");
    };
    let (mut ipc, mut cap, mut conv, mut enc, mut cfg) = (vec![], vec![], vec![], vec![], vec![]);
    let mut size = 0;
    for _ in 0..args.n.max(1) {
        let t = Instant::now();
        instance.cursor()?;
        instance.query("clients")?;
        ipc.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let frame = wl.capture(&monitor.name, None)?;
        cap.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let rgb = frame.rgb()?;
        conv.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let q = qoi::encode_to_vec(&rgb, frame.width, frame.height)?;
        enc.push(t.elapsed().as_secs_f64() * 1e3);
        size = q.len();

        let t = Instant::now();
        config::Config::load(&instance)?;
        cfg.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("{} ({}x{} logical), {} samples; QOI frame {} KiB", monitor.name, monitor.width, monitor.height, args.n, size / 1024);
    report("ipc cursor+clients", &mut ipc);
    report("capture", &mut cap);
    report("to rgb", &mut conv);
    report("qoi encode", &mut enc);
    report("config load", &mut cfg);
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args()?;
    discover_wayland();
    match args.command.as_str() {
        "serve" => serve(&args),
        "doctor" => doctor(),
        "bench" => bench(&args),
        _ => {
            eprintln!("usage: hyprhands serve [--monitor NAME] [--tolerance PX] | doctor | bench [--monitor NAME] [-n N]");
            Ok(())
        }
    }
}
