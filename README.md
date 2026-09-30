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
hyprhands serve [--monitor NAME]     the protocol (see src/proto.rs)
ssh my-desktop hyprhands serve       the same, from another machine
```

## Takeover

The owner is at the controls when the cursor drifts more than `--tolerance`
(64) logical px from where hyprhands left it, or focus moves to another
monitor. The first sign latches and every later input is refused. Creating
`$XDG_RUNTIME_DIR/hyprhands-stop` does the same, explicitly.

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
