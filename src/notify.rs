//! Desktop notifications for the owner: one notice per session (started, stopped, ended), updated
//! in place through `org.freedesktop.Notifications` on the session bus (swaync, mako, dunst...).
//! A notification that cannot be sent is dropped: it is feedback, never a reason to fail.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use zbus::zvariant::Value;

#[derive(Clone)]
pub struct Notifier {
    conn: Option<zbus::Connection>,
    /// The notice to replace, so a session is one notification however often it changes.
    id: Arc<Mutex<u32>>,
}

impl Notifier {
    pub fn connect(enabled: bool) -> Self {
        let conn = enabled
            .then(|| zbus::block_on(zbus::Connection::session()).ok())
            .flatten();
        Self {
            conn,
            id: Arc::default(),
        }
    }

    /// Show `summary`/`body`, replacing this session's previous notice. `expire_ms` 0 keeps it
    /// until dismissed; `critical` asks the daemon to keep it up and style it as urgent.
    pub fn show(&self, summary: &str, body: &str, expire_ms: i32, critical: bool) {
        let Some(conn) = &self.conn else { return };
        let replaces = *self.id.lock().unwrap();
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("urgency", Value::U8(if critical { 2 } else { 1 }));
        hints.insert("desktop-entry", Value::from("hyprhands"));
        let reply = zbus::block_on(conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "hyprhands",
                replaces,
                "input-mouse",
                summary,
                body,
                Vec::<&str>::new(),
                hints,
                expire_ms,
            ),
        ));
        if let Ok(id) = reply.and_then(|m| m.body().deserialize::<u32>()) {
            *self.id.lock().unwrap() = id;
        }
    }
}
