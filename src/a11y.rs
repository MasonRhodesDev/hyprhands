//! The accessibility tree of the agent's window, read in-process over the AT-SPI bus.
//!
//! A walk is thousands of D-Bus calls. They go out a breadth-first level at a time, every node of
//! the level at once on one connection, so a level costs about one round trip instead of one per
//! call. The walk is capped by nodes and time, and descends only into what is showing (or holds
//! focus), as typesafe-computer-use's `remote.py` did, which this ports.
//!
//! Every frame is in the driven monitor's logical space, like every other coordinate the server
//! hands out. The tree serializes as XML in the shape OSWorld's server writes, so jev's
//! `osworld/a11y.py` reads it unchanged.

use anyhow::{Context, Result, anyhow};
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::text::TextProxy;
use atspi::{AccessibilityConnection, CoordType, Interface, ObjectRefOwned, State};
use futures_util::future::join_all;
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use zbus::{fdo::DBusProxy, names::BusName, proxy::CacheProperties};

pub const NODE_CAP: usize = 3000;
pub const TIME_CAP: Duration = Duration::from_millis(1500);
const TEXT_CHARS: i32 = 500;
const EXTENT_SANITY: i32 = 20000;

const NS_STATE: &str = "https://accessibility.ubuntu.example.org/ns/state";
const NS_ATTRIBUTES: &str = "https://accessibility.ubuntu.example.org/ns/attributes";
const NS_COMPONENT: &str = "https://accessibility.ubuntu.example.org/ns/component";
const NS_ACTION: &str = "https://accessibility.ubuntu.example.org/ns/action";

const VIEWPORT_ROLES: [&str; 2] = ["document-web", "document-frame"];
/// Roles whose text content is read; for everything else only the name is.
const TEXT_TAGS: [&str; 9] = [
    "entry",
    "text",
    "static",
    "label",
    "paragraph",
    "heading",
    "link",
    "list-item",
    "table-cell",
];

/// One element, in an arena: `children` index into the same Vec as the node itself.
#[derive(Debug, Default, Clone)]
pub struct Node {
    pub role: String,
    pub name: String,
    pub states: Vec<String>,
    /// x, y, w, h in the driven monitor's logical space; only for a showing, visible element.
    pub frame: Option<[f64; 4]>,
    pub actions: Vec<(String, String)>,
    pub attributes: Vec<(&'static str, String)>,
    pub text: String,
    pub children: Vec<usize>,
}

impl Node {
    pub fn has(&self, state: &str) -> bool {
        self.states.iter().any(|s| s == state)
    }
}

/// The walked tree: node 0 is the `desktop-frame` root, node 1 (when present) the application.
#[derive(Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    /// Elements read below the window, not counting the root, application and window.
    pub count: usize,
    pub capped: bool,
    /// The window's app is not on the bus: it runs without accessibility, or has not registered yet.
    pub no_app: bool,
}

impl Tree {
    fn empty() -> Self {
        Self {
            nodes: vec![Node {
                role: "desktop-frame".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn push(&mut self, parent: usize, node: Node) -> usize {
        self.nodes.push(node);
        let id = self.nodes.len() - 1;
        self.nodes[parent].children.push(id);
        id
    }

    /// The XML of a tree with nothing in it.
    pub fn default_xml() -> String {
        Self::empty().xml()
    }

    pub fn app(&self) -> Option<usize> {
        self.nodes[0].children.first().copied()
    }

    pub fn xml(&self) -> String {
        let mut out = String::with_capacity(self.nodes.len() * 160);
        write_xml(self, 0, true, &mut out);
        out
    }
}

/// The window a walk starts from, as the server knows it from Hyprland.
pub struct Target<'a> {
    pub pid: u32,
    pub title: &'a str,
    pub class: &'a str,
    /// The window's top-left in the driven monitor's logical space.
    pub origin: (f64, f64),
    /// The window's logical width, and its monitor's scale: see `physical_document`.
    pub width: f64,
    pub scale: f64,
}

/// Chromium and Electron on a fractionally scaled output report a web document's extents in
/// buffer pixels while their own frame and panels stay logical: a document the scale times the
/// size of the logical view holding it (its parent, else the window) is one. Everything under it
/// is divided back down.
fn physical_document(node: &Node, parent: Option<[f64; 4]>, target: &Target) -> bool {
    let Some([_, _, w, h]) = node.frame else {
        return false;
    };
    let near = |ratio: f64| (ratio - target.scale).abs() < 0.08 * target.scale;
    let held = match parent {
        Some([_, _, pw, ph]) if pw > 0.0 && ph > 0.0 => near(w / pw) && near(h / ph),
        _ => target.width > 0.0 && near(w / target.width),
    };
    target.scale > 1.01 && VIEWPORT_ROLES.contains(&node.role.as_str()) && held
}

/// `fut`'s output, or None once `deadline` passes: a hung app on the bus would otherwise hold the
/// walk (and the single-threaded server) for D-Bus's 25 s method timeout.
async fn before<F: std::future::Future>(deadline: Instant, fut: F) -> Option<F::Output> {
    match futures_util::future::select(Box::pin(fut), Box::pin(async_io::Timer::at(deadline))).await
    {
        futures_util::future::Either::Left((out, _)) => Some(out),
        futures_util::future::Either::Right(_) => None,
    }
}

fn rescale(frame: &mut Option<[f64; 4]>, origin: (f64, f64), k: f64) {
    if let Some([x, y, w, h]) = frame {
        *frame = Some([
            origin.0 + (*x - origin.0) / k,
            origin.1 + (*y - origin.1) / k,
            *w / k,
            *h / k,
        ]);
    }
}

pub struct A11y {
    conn: zbus::Connection,
}

impl A11y {
    /// Connects to the accessibility bus the session bus's broker names (over ssh, a stale bus
    /// socket in the environment would otherwise win).
    pub fn connect() -> Result<Self> {
        let conn = zbus::block_on(AccessibilityConnection::new()).context("no AT-SPI bus")?;
        Ok(Self {
            conn: conn.connection().clone(),
        })
    }

    /// Applications registered on the bus.
    pub fn app_count(&self) -> Result<usize> {
        zbus::block_on(async {
            Ok(self
                .accessible(&registry_root())
                .await?
                .get_children()
                .await?
                .len())
        })
    }

    pub fn tree(&self, target: &Target) -> Result<Tree> {
        zbus::block_on(self.walk(target, Instant::now() + TIME_CAP))
    }

    async fn accessible(&self, at: &ObjectRefOwned) -> zbus::Result<AccessibleProxy<'static>> {
        let name = at
            .name_as_str()
            .ok_or(zbus::Error::MissingParameter("null object reference"))?
            .to_owned();
        AccessibleProxy::builder(&self.conn)
            .destination(name)?
            .path(at.path_as_str().to_owned())?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn app_for_pid(&self, pid: u32) -> Result<Option<ObjectRefOwned>> {
        let apps = self
            .accessible(&registry_root())
            .await?
            .get_children()
            .await?;
        let bus = DBusProxy::new(&self.conn).await?;
        let pids = join_all(apps.iter().map(|app| {
            let bus = &bus;
            async move {
                let name = BusName::try_from(app.name_as_str()?.to_owned()).ok()?;
                bus.get_connection_unix_process_id(name).await.ok()
            }
        }))
        .await;
        Ok(apps
            .into_iter()
            .zip(pids)
            .find(|(_, p)| *p == Some(pid))
            .map(|(a, _)| a))
    }

    /// The app's frame titled like the window, else its active one, else its first.
    async fn window_of(
        &self,
        app: &AccessibleProxy<'_>,
        title: &str,
    ) -> Result<Option<ObjectRefOwned>> {
        let frames = app.get_children().await?;
        let facts = join_all(frames.iter().map(|f| async move {
            let Ok(p) = self.accessible(f).await else {
                return (String::new(), false);
            };
            let (name, state) = futures_util::join!(p.name(), p.get_state());
            (
                name.unwrap_or_default(),
                state.is_ok_and(|s| s.contains(State::Active)),
            )
        }))
        .await;
        let pick = facts
            .iter()
            .position(|(n, _)| n == title)
            .or_else(|| facts.iter().position(|(_, active)| *active));
        Ok(pick
            .or((!frames.is_empty()).then_some(0))
            .map(|i| frames[i].clone()))
    }

    async fn walk(&self, target: &Target<'_>, deadline: Instant) -> Result<Tree> {
        let mut tree = Tree::empty();
        let Some(found) = before(deadline, self.app_for_pid(target.pid)).await else {
            tree.capped = true;
            return Ok(tree);
        };
        let Some(app_ref) = found? else {
            tree.no_app = true;
            return Ok(tree);
        };
        let app = self.accessible(&app_ref).await?;
        let Some(app_name) = before(deadline, app.name()).await else {
            tree.capped = true;
            return Ok(tree);
        };
        let app_name = app_name
            .ok()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| target.class.to_owned());
        let app_id = tree.push(
            0,
            Node {
                role: "application".into(),
                name: app_name,
                ..Default::default()
            },
        );
        let Some(window) = before(deadline, self.window_of(&app, target.title)).await else {
            tree.capped = true;
            return Ok(tree);
        };
        let Some(window) = window? else {
            return Ok(tree);
        };
        let Some(win) = before(deadline, self.read(&window, target.origin, false)).await else {
            tree.capped = true;
            return Ok(tree);
        };
        let (mut win, _) = win.map_err(|e| anyhow!("reading the window: {e}"))?;
        // Hyprland knows which window has the keyboard; AT-SPI on Wayland often does not say.
        if !win.has("active") {
            win.states.push("active".into());
        }
        let win_id = tree.push(app_id, win);

        // Each entry carries the divisor its extents need (1 until a physical-pixel document).
        let mut level = vec![(window, win_id, 1.0)];
        while !level.is_empty() {
            if Instant::now() > deadline {
                tree.capped = true;
                break;
            }
            let kids = join_all(level.iter().map(|(at, _, _)| async move {
                match self.accessible(at).await {
                    Ok(p) => p.get_children().await.unwrap_or_default(),
                    Err(_) => vec![],
                }
            }));
            let Some(kids) = before(deadline, kids).await else {
                tree.capped = true;
                break;
            };
            let mut wanted: Vec<(ObjectRefOwned, usize, f64)> = vec![];
            for ((_, parent, k), refs) in level.iter().zip(kids) {
                for r in refs.into_iter().filter(|r| r.name_as_str().is_some()) {
                    if tree.count + wanted.len() >= NODE_CAP {
                        tree.capped = true;
                        break;
                    }
                    wanted.push((r, *parent, *k));
                }
            }
            let read = join_all(
                wanted
                    .iter()
                    .map(|(at, _, _)| self.read(at, target.origin, true)),
            );
            let Some(read) = before(deadline, read).await else {
                tree.capped = true;
                break;
            };
            let mut next = vec![];
            for ((at, parent, mut k), node) in wanted.into_iter().zip(read) {
                let Ok((mut node, descend)) = node else {
                    continue;
                };
                if k == 1.0 && physical_document(&node, tree.nodes[parent].frame, target) {
                    k = target.scale;
                }
                if k != 1.0 {
                    rescale(&mut node.frame, target.origin, k);
                }
                tree.count += 1;
                let id = tree.push(parent, node);
                if descend {
                    next.push((at, id, k));
                }
            }
            if tree.capped {
                break;
            }
            level = next;
        }
        Ok(tree)
    }

    /// One element's facts, and whether the walk descends into it. `text_roles` limits reading
    /// text content to TEXT_TAGS (the window itself reads none).
    async fn read(
        &self,
        at: &ObjectRefOwned,
        origin: (f64, f64),
        text_roles: bool,
    ) -> zbus::Result<(Node, bool)> {
        let p = self.accessible(at).await?;
        let (role, name, state, ifaces, attrs) = futures_util::join!(
            p.get_role_name(),
            p.name(),
            p.get_state(),
            p.get_interfaces(),
            p.get_attributes()
        );
        let role = role_tag(&role?);
        let ifaces = ifaces.unwrap_or_default();
        let states: Vec<String> = state
            .map(|s| s.iter().map(state_name).collect())
            .unwrap_or_default();
        let showing = states.iter().any(|s| s == "showing");
        let keep_text = text_roles && TEXT_TAGS.contains(&role.as_str()) && role != "password-text";

        let (dest, path) = (
            p.inner().destination().to_owned(),
            p.inner().path().to_owned(),
        );
        let extents = async {
            if !(showing
                && states.iter().any(|s| s == "visible")
                && ifaces.contains(Interface::Component))
            {
                return None;
            }
            let c = ComponentProxy::builder(&self.conn)
                .destination(dest.clone())
                .ok()?
                .path(path.clone())
                .ok()?
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .ok()?;
            let (x, y, w, h) = c.get_extents(CoordType::Window).await.ok()?;
            (w > 0 && h > 0 && x.abs() < EXTENT_SANITY && y.abs() < EXTENT_SANITY).then(|| {
                [
                    origin.0 + f64::from(x),
                    origin.1 + f64::from(y),
                    f64::from(w),
                    f64::from(h),
                ]
            })
        };
        let actions = async {
            if !ifaces.contains(Interface::Action) {
                return vec![];
            }
            let Some(a) = async {
                ActionProxy::builder(&self.conn)
                    .destination(dest.clone())
                    .ok()?
                    .path(path.clone())
                    .ok()?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await
                    .ok()
            }
            .await
            else {
                return vec![];
            };
            a.get_actions()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|a| {
                    !a.name.is_empty()
                        && a.name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .map(|a| {
                    let desc = if a.description.is_empty() {
                        a.name.clone()
                    } else {
                        a.description
                    };
                    (a.name, desc)
                })
                .collect()
        };
        let text = async {
            if !(keep_text && ifaces.contains(Interface::Text)) {
                return String::new();
            }
            let Some(t) = async {
                TextProxy::builder(&self.conn)
                    .destination(dest.clone())
                    .ok()?
                    .path(path.clone())
                    .ok()?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await
                    .ok()
            }
            .await
            else {
                return String::new();
            };
            match t.character_count().await {
                // An embedded object's placeholder (U+FFFC) alone is not text anyone can read.
                Ok(n) if n > 0 => t
                    .get_text(0, n.min(TEXT_CHARS))
                    .await
                    .ok()
                    .filter(|s| s.chars().any(|c| c != '\u{FFFC}' && !c.is_whitespace()))
                    .unwrap_or_default(),
                _ => String::new(),
            }
        };
        let (frame, actions, text) = futures_util::join!(extents, actions, text);

        let attrs = attrs.unwrap_or_default();
        let attributes = ["placeholder", "placeholder-text"]
            .into_iter()
            .filter_map(|k| {
                attrs
                    .get(k)
                    .filter(|v| !v.is_empty())
                    .map(|v| (k, v.clone()))
            })
            .collect();
        let descend = showing || states.iter().any(|s| s == "focused");
        Ok((
            Node {
                role,
                name: name.unwrap_or_default(),
                states,
                frame,
                actions,
                attributes,
                text,
                children: vec![],
            },
            descend,
        ))
    }
}

fn registry_root() -> ObjectRefOwned {
    ObjectRefOwned::from_static_str_unchecked(
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
    )
}

/// The XML tag for a role name: hyphens for spaces, and GTK 4 / newer Chromium's "button" under
/// the "push-button" OSWorld's GNOME tree (which jev maps) uses.
fn role_tag(role: &str) -> String {
    let tag = role.trim().replace(' ', "-");
    match tag.as_str() {
        "" => "unknown".into(),
        "button" => "push-button".into(),
        t if t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => tag,
        _ => "unknown".into(),
    }
}

/// A state's nick as OSWorld writes it: `multi_line`, not `multi-line`.
fn state_name(s: State) -> String {
    s.to_static_str().replace('-', "_")
}

fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            // XML 1.0 cannot carry other control characters, even escaped.
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
}

fn write_xml(tree: &Tree, id: usize, root: bool, out: &mut String) {
    let n = &tree.nodes[id];
    out.push('<');
    out.push_str(&n.role);
    if root {
        let _ = write!(
            out,
            r#" xmlns:st="{NS_STATE}" xmlns:attr="{NS_ATTRIBUTES}" xmlns:cp="{NS_COMPONENT}" xmlns:act="{NS_ACTION}""#
        );
    }
    if !n.name.is_empty() {
        out.push_str(" name=\"");
        escape(&n.name, out);
        out.push('"');
    }
    for s in &n.states {
        let _ = write!(out, r#" st:{s}="true""#);
    }
    if let Some([x, y, w, h]) = n.frame {
        let _ = write!(out, r#" cp:screencoord="({x}, {y})" cp:size="({w}, {h})""#);
    }
    for (name, desc) in &n.actions {
        let _ = write!(out, r#" act:{name}_desc=""#);
        escape(desc, out);
        out.push('"');
    }
    for (key, value) in &n.attributes {
        let _ = write!(out, r#" attr:{key}=""#);
        escape(value, out);
        out.push('"');
    }
    if n.text.is_empty() && n.children.is_empty() {
        out.push_str(" />");
        return;
    }
    out.push('>');
    escape(&n.text, out);
    for &kid in &n.children {
        write_xml(tree, kid, false, out);
    }
    let _ = write!(out, "</{}>", n.role);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_physical_pixel_document_is_scaled_back_to_logical() {
        let target = Target {
            pid: 1,
            title: "",
            class: "",
            origin: (9.0, 49.0),
            width: 1422.0,
            scale: 1.5,
        };
        let mut doc = Node {
            role: "document-web".into(),
            frame: Some([9.0, 49.0, 2133.0, 1863.0]),
            ..Default::default()
        };
        let panel = Some([9.0, 49.0, 1422.0, 1242.0]);
        assert!(physical_document(&doc, panel, &target));
        assert!(physical_document(&doc, None, &target));
        // A browser's page beside a sidebar: narrower than the window, but 1.5x its own view.
        let page = Node {
            role: "document-web".into(),
            frame: Some([309.0, 149.0, 1683.0, 1710.0]),
            ..Default::default()
        };
        assert!(physical_document(
            &page,
            Some([309.0, 149.0, 1122.0, 1140.0]),
            &target
        ));
        rescale(&mut doc.frame, target.origin, 1.5);
        assert_eq!(doc.frame, Some([9.0, 49.0, 1422.0, 1242.0]));
        let mut help = Some([9.0 + 1913.0, 49.0 + 55.0, 36.0, 36.0]);
        rescale(&mut help, target.origin, 1.5);
        assert_eq!(
            help.map(|f| f.map(|v| (v * 10.0).round() / 10.0)),
            Some([1284.3, 85.7, 24.0, 24.0])
        );
        let logical = Node {
            role: "document-web".into(),
            frame: Some([9.0, 120.0, 1422.0, 1100.0]),
            ..Default::default()
        };
        assert!(!physical_document(
            &logical,
            Some([9.0, 120.0, 1422.0, 1100.0]),
            &target
        ));
        assert!(!physical_document(
            &doc,
            panel,
            &Target {
                scale: 1.0,
                ..target
            }
        ));
    }

    #[test]
    fn roles_become_osworld_tags() {
        assert_eq!(role_tag("push button"), "push-button");
        assert_eq!(role_tag("button"), "push-button");
        assert_eq!(role_tag("document web"), "document-web");
        assert_eq!(role_tag("weird/role"), "unknown");
        assert_eq!(role_tag(""), "unknown");
    }

    #[test]
    fn xml_carries_namespaces_escapes_and_frames() {
        let mut t = Tree::empty();
        let app = t.push(
            0,
            Node {
                role: "application".into(),
                name: "Firefox".into(),
                ..Default::default()
            },
        );
        t.push(
            app,
            Node {
                role: "heading".into(),
                name: "Tom & \"Jerry\"".into(),
                states: vec!["showing".into(), "visible".into()],
                frame: Some([10.0, 20.5, 300.0, 18.0]),
                actions: vec![("click".into(), "click".into())],
                text: "a < b\u{1}".into(),
                ..Default::default()
            },
        );
        let xml = t.xml();
        assert!(xml.starts_with(
            r#"<desktop-frame xmlns:st="https://accessibility.ubuntu.example.org/ns/state""#
        ));
        assert!(xml.contains(r#"<heading name="Tom &amp; &quot;Jerry&quot;" st:showing="true" st:visible="true" cp:screencoord="(10, 20.5)" cp:size="(300, 18)" act:click_desc="click">a &lt; b</heading>"#));
        assert!(xml.ends_with("</application></desktop-frame>"));
        assert_eq!(
            Tree::empty().xml(),
            format!(
                r#"<desktop-frame xmlns:st="{NS_STATE}" xmlns:attr="{NS_ATTRIBUTES}" xmlns:cp="{NS_COMPONENT}" xmlns:act="{NS_ACTION}" />"#
            )
        );
    }
}
