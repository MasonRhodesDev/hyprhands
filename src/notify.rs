//! Desktop notifications for the owner: one notice per session, updated in place through
//! `org.freedesktop.Notifications` on the session bus (swaync, mako, dunst...), and never left
//! behind: it says "driving" while the session runs, becomes a short-lived "stopped" or "done",
//! and a session that dies without either has its notice closed by its signal handler or, after a
//! crash, by the next session (the id is kept in `$XDG_RUNTIME_DIR/hyprhands-notification`).
//! A notification that cannot be sent is dropped: it is feedback, never a reason to fail.

use crate::hypr;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zbus::zvariant::Value;

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// How long a notice lasts, in the daemon's terms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lasts {
    /// Until replaced or closed: the "driving" notice, for as long as the session runs.
    Session,
    /// Expires by itself after this many ms.
    For(i32),
}

#[derive(Clone)]
pub struct Notifier {
    conn: Option<zbus::Connection>,
    /// The notice to replace, so a session is one notification however often it changes.
    id: Arc<Mutex<u32>>,
}

fn id_file() -> PathBuf {
    hypr::runtime_dir().join("hyprhands-notification")
}

impl Notifier {
    /// Connects, and closes the notice an earlier session left up if it never got to.
    pub fn connect(enabled: bool) -> Self {
        let conn = enabled.then(|| zbus::block_on(zbus::Connection::session()).ok()).flatten();
        let n = Self { conn, id: Arc::default() };
        if let Some(old) = std::fs::read_to_string(id_file()).ok().and_then(|s| s.trim().parse().ok()) {
            n.close_id(old);
        }
        let _ = std::fs::remove_file(id_file());
        n
    }

    /// Show `summary`/`body`, replacing this session's previous notice.
    pub fn show(&self, summary: &str, body: &str, lasts: Lasts) {
        let Some(conn) = &self.conn else { return };
        let replaces = *self.id.lock().unwrap();
        let mut hints: HashMap<&str, Value> = HashMap::new();
        // Normal urgency throughout: most daemons keep a critical notice up whatever its timeout.
        hints.insert("urgency", Value::U8(1));
        hints.insert("desktop-entry", Value::from("hyprhands"));
        match lasts {
            Lasts::Session => {
                hints.insert("resident", Value::Bool(true));
            }
            // Gone once it expires: a daemon with a notification centre (swaync) otherwise keeps
            // every expired notice in its history, and they pile up session after session.
            Lasts::For(_) => {
                hints.insert("transient", Value::Bool(true));
            }
        }
        let expire = match lasts {
            Lasts::Session => 0,
            Lasts::For(ms) => ms,
        };
        let reply = zbus::block_on(conn.call_method(
            Some(DEST),
            PATH,
            Some(DEST),
            "Notify",
            &("hyprhands", replaces, "input-mouse", summary, body, Vec::<&str>::new(), hints, expire),
        ));
        if let Ok(id) = reply.and_then(|m| m.body().deserialize::<u32>()) {
            *self.id.lock().unwrap() = id;
            // Only a notice that would otherwise stay needs someone to close it after a crash.
            if lasts == Lasts::Session {
                let _ = std::fs::write(id_file(), id.to_string());
            } else {
                let _ = std::fs::remove_file(id_file());
            }
        }
    }

    /// Take this session's notice down now (a signal is ending the process).
    pub fn close(&self) {
        let id = std::mem::take(&mut *self.id.lock().unwrap());
        if id != 0 {
            self.close_id(id);
        }
        let _ = std::fs::remove_file(id_file());
    }

    fn close_id(&self, id: u32) {
        if let Some(conn) = &self.conn {
            let _ = zbus::block_on(conn.call_method(Some(DEST), PATH, Some(DEST), "CloseNotification", &(id,)));
        }
    }
}
