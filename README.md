# hyprhands

Fast, safe hands on a Hyprland desktop for agent loops: screen capture, the
accessibility tree, pointer and keyboard input, and a watch for the owner
taking the seat back. One long-lived process per session, speaking a framed
protocol on stdio, so it runs locally or over ssh unchanged.

It never writes the owner's config. It reads the loaded binds and input
options over IPC to know what keys do, and refuses to send a combo the
compositor would swallow as a bind unless asked to.

```
hyprhands doctor                     what this session can and cannot do
hyprhands bench [--monitor NAME] [--class C]
                                     read-only timings, sends no input
hyprhands stop                       refuse all input until the next session
hyprhands serve [--monitor NAME]     the protocol (see src/proto.rs)
ssh my-desktop hyprhands serve       the same, from another machine
```

## Takeover

The owner is at the controls when the cursor drifts more than `--tolerance`
(64) logical px from where hyprhands left it, or focus moves to another
monitor. The first sign latches and every later input is refused. Creating
`$XDG_RUNTIME_DIR/hyprhands-stop` does the same, explicitly.

## What the owner sees

While a session drives a monitor, hyprhands makes it unmistakable (layer-shell overlay,
click-through, gone with the process):

- a tape frame whose stripes keep marching;
- the pointer becomes a robot cursor, on every shape of the owner's theme (arrow, hand, I-beam...);
- a ripple where each click lands, like a drop in water, and a tail along each jump of the
  pointer, drawn out and fading from its end, in its own colour;
- a caption saying what it is doing (a request's `caption` field, or the op itself);
- one desktop notification per session: driving, then a short-lived stopped or done, never left
  behind;
- with `--tint`, a wash over every monitor of the seat.

The look is a theme: `construction` (the default) is 8-bit hazard tape in hi-vis yellow and
black, a lime ripple and a safety-orange tail, barrier-tape red and white once stopped;
`adaptive` is smooth, in the complement of the owner's window-border colour. `serve --theme
NAME|FILE` picks one; an owner's own is a TOML file in `~/.config/hyprhands/themes/` (see
src/theme.rs) that extends a built-in.

The overlay is hidden for each capture, so agents never see it (about one frame per
screenshot). `serve --no-overlay` and `--no-notify` turn it off; `hyprhands overlay-check`
shows it and confirms captures stay clean; `tests/qa` holds the hypr-qa scenarios that check
all of it in a VM.

## Accessibility tree and screen text

`{"op": "tree", "text": true}` walks the agent window's AT-SPI tree in-process
(a few pipelined D-Bus round trips per breadth-first level, capped at 3000
nodes and 1.5 s, which also bounds an app that stops answering)
and returns it as OSWorld-shaped XML, with every frame in the driven monitor's
logical space. With `text`, it also returns the visible strings as lines in
capture pixels, standing in for OCR, or null when the tree is too thin to.
`address` walks another window instead; it only reads, so it moves nothing.

Chromium and Electron report web content in buffer pixels on a fractionally
scaled monitor; hyprhands notices the oversized document and scales it back.
Apps without accessibility (kitty, most games) come back with `no_app`; with
no AT-SPI bus at all, every tree is empty and `a11y` is false.

## Status

Phase 1 (IPC, capture, input, takeover, config) and phase 2 (accessibility
tree, screen text). See NOTICE.md for what this learned from hypruse and
typesafe-computer-use.
