//! The screen's text read from the accessibility tree instead of from pixels.
//!
//! A browser page with its tree on (and most GTK and Qt apps) already publishes every visible
//! string with its exact frame. Reading those takes no time and misreads nothing, where OCR costs
//! seconds a capture. When the tree carries too little text (a terminal, a canvas, a thin tree)
//! there are no lines, and the client reads pixels as before.
//!
//! Only the deepest text is taken: a paragraph whose static-text children already said everything
//! is not said again as one block. Field contents are never read, and a password field is skipped
//! outright. Ported from typesafe-computer-use's `hyprland/tree_text.py`.

use crate::a11y::Tree;
use serde::Serialize;

/// Roles that are text, not controls. A control already reaches the agent as an element with its
/// label; offering it as a text line too lets OCR-tuned block merging glue stacked controls into
/// one block aimed between them. A link's words still arrive through its static-text children.
const TEXT_ROLES: [&str; 8] = [
    "static",
    "label",
    "text",
    "heading",
    "paragraph",
    "caption",
    "list-item",
    "table-cell",
];
const NEVER: [&str; 2] = ["entry", "password-text"];
/// A "line" taller than this share of the display is a container, not text.
const MAX_HEIGHT_SHARE: f64 = 0.4;
/// Thinner than this is visually hidden screen-reader text (sites clip it to 1px), never drawn.
const MIN_SIDE: f64 = 4.0;
/// Hidden text not clipped to a pixel is laid out in a sliver, a letter or two a row: a column far
/// taller than wide. Real horizontal text is never shaped like that.
const COLUMN_MIN_HEIGHT: f64 = 60.0;
const COLUMN_RATIO: f64 = 2.0;
/// Fewer strings than this and the tree is too thin to stand in for OCR.
pub const MIN_LINES: usize = 8;

/// Controls drawn on top of text they overlap: a sticky header's links cover the page scrolling
/// under it, and AT-SPI still reports the covered text as showing.
const COVER_ROLES: [&str; 9] = [
    "push-button",
    "toggle-button",
    "link",
    "entry",
    "combo-box",
    "page-tab",
    "menu-item",
    "check-box",
    "radio-button",
];
/// A web page: its text is only visible inside its frame.
const VIEWPORT_ROLES: [&str; 2] = ["document-web", "document-frame"];
/// Share of a text box a control must cover to hide it.
const MIN_COVER: f64 = 0.5;

/// x1, y1, x2, y2.
type Box = [f64; 4];

#[derive(Debug, Serialize, PartialEq)]
pub struct Line {
    pub text: String,
    /// x1, y1, x2, y2 in capture pixels.
    #[serde(rename = "box")]
    pub bounds: Box,
}

fn to_box([x, y, w, h]: [f64; 4]) -> Box {
    [x, y, x + w, y + h]
}

fn clip(a: Box, b: Box) -> Option<Box> {
    let c = [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ];
    (c[2] > c[0] && c[3] > c[1]).then_some(c)
}

fn area(b: Box) -> f64 {
    (b[2] - b[0]) * (b[3] - b[1])
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Found {
    text: String,
    bounds: Box,
    ancestors: Vec<usize>,
}

/// Visible leaf strings under the tree's application, as lines in capture pixels (logical points
/// times `scale`), for a display of `display_w` by `display_h` points.
///
/// Each string is clipped to the display and to the web page's viewport, since Chromium reports a
/// heading scrolled half out of view with the frame it would have unclipped. A string mostly
/// covered by a control that is not its own ancestor is dropped: the control is what is drawn
/// there, and what a click there would hit.
pub fn lines(tree: &Tree, scale: f64, display_w: f64, display_h: f64) -> Vec<Line> {
    let Some(app) = tree.app() else { return vec![] };
    let display = [0.0, 0.0, display_w, display_h];
    let covers: Vec<(Box, usize)> = (0..tree.nodes.len())
        .filter_map(|id| {
            let n = &tree.nodes[id];
            let f = n.frame?;
            (COVER_ROLES.contains(&n.role.as_str()) && f[2] > 0.0 && f[3] > 0.0)
                .then(|| (to_box(f), id))
        })
        .collect();
    let mut found = vec![];
    let mut path = vec![];
    visit(tree, app, display, display_h, &mut path, &mut found);
    found
        .into_iter()
        .filter(|f| {
            !covers.iter().any(|(cover, id)| {
                !f.ancestors.contains(id)
                    && clip(f.bounds, *cover)
                        .is_some_and(|hit| area(hit) >= MIN_COVER * area(f.bounds))
            })
        })
        .map(|f| Line {
            text: f.text,
            bounds: f.bounds.map(|v| v * scale),
        })
        .collect()
}

/// Emits the deepest text; true when this subtree emitted anything.
fn visit(
    tree: &Tree,
    id: usize,
    mut view: Box,
    display_h: f64,
    path: &mut Vec<usize>,
    found: &mut Vec<Found>,
) -> bool {
    let n = &tree.nodes[id];
    if NEVER.contains(&n.role.as_str()) {
        return false;
    }
    if VIEWPORT_ROLES.contains(&n.role.as_str())
        && let Some(f) = n.frame
    {
        view = clip(view, to_box(f)).unwrap_or(view);
    }
    path.push(id);
    let mut below = false;
    for &kid in &n.children {
        below |= visit(tree, kid, view, display_h, path, found);
    }
    path.pop();
    if below {
        return true;
    }
    let Some(frame) = n.frame.filter(|_| TEXT_ROLES.contains(&n.role.as_str())) else {
        return false;
    };
    let text = match collapse(&n.name) {
        name if !name.is_empty() => name,
        _ => collapse(&n.text),
    };
    // An embedded object's placeholder (U+FFFC) alone is not text anyone can read.
    if !text.chars().any(|c| c != '\u{FFFC}' && !c.is_whitespace()) {
        return false;
    }
    let Some(b) = clip(to_box(frame), view) else {
        return false;
    };
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    if text.is_empty()
        || w.min(h) < MIN_SIDE
        || h > MAX_HEIGHT_SHARE * display_h
        || (h > COLUMN_MIN_HEIGHT && h > COLUMN_RATIO * w)
    {
        return false;
    }
    found.push(Found {
        text,
        bounds: b,
        ancestors: path.clone(),
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a11y::Node;

    fn node(role: &str, name: &str, frame: Option<[f64; 4]>) -> Node {
        Node {
            role: role.into(),
            name: name.into(),
            frame,
            ..Default::default()
        }
    }

    /// root → app → doc(frame) → [kids...]
    fn page(kids: Vec<Node>) -> Tree {
        let mut nodes = vec![
            Node {
                role: "desktop-frame".into(),
                children: vec![1],
                ..Default::default()
            },
            Node {
                role: "application".into(),
                children: vec![2],
                ..Default::default()
            },
            Node {
                role: "document-web".into(),
                frame: Some([0.0, 100.0, 1000.0, 800.0]),
                ..Default::default()
            },
        ];
        for k in kids {
            nodes.push(k);
            let id = nodes.len() - 1;
            nodes[2].children.push(id);
        }
        Tree {
            nodes,
            ..Default::default()
        }
    }

    #[test]
    fn deepest_text_is_clipped_to_the_viewport_and_scaled() {
        let mut t = page(vec![
            node(
                "heading",
                "Half   scrolled",
                Some([10.0, 80.0, 200.0, 40.0]),
            ),
            node("paragraph", "", Some([10.0, 300.0, 400.0, 60.0])),
        ]);
        // The paragraph's static child says its words, so the paragraph is not said again.
        t.nodes.push(node(
            "static",
            "child words",
            Some([10.0, 300.0, 400.0, 20.0]),
        ));
        t.nodes[4].children.push(5);
        let got = lines(&t, 2.0, 1000.0, 1000.0);
        assert_eq!(
            got,
            vec![
                Line {
                    text: "Half scrolled".into(),
                    bounds: [20.0, 200.0, 420.0, 240.0]
                },
                Line {
                    text: "child words".into(),
                    bounds: [20.0, 600.0, 820.0, 640.0]
                },
            ]
        );
    }

    #[test]
    fn hidden_covered_and_field_text_is_dropped() {
        let mut entry = node("entry", "typed secret", Some([0.0, 500.0, 300.0, 20.0]));
        entry.text = "typed secret".into();
        let t = page(vec![
            node("static", "sr-only", Some([0.0, 200.0, 1.0, 1.0])),
            node("static", "sliver", Some([0.0, 200.0, 10.0, 200.0])),
            node("static", "under header", Some([0.0, 120.0, 100.0, 20.0])),
            node("link", "", Some([0.0, 110.0, 100.0, 40.0])),
            entry,
            node("label", "kept", Some([500.0, 500.0, 100.0, 20.0])),
            node("static", "\u{FFFC}", Some([500.0, 600.0, 100.0, 20.0])),
        ]);
        let got: Vec<_> = lines(&t, 1.0, 1000.0, 1000.0)
            .into_iter()
            .map(|l| l.text)
            .collect();
        assert_eq!(got, vec!["kept"]);
    }
}
