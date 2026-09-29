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
hyprhands bench [--monitor NAME]     read-only timings, sends no input
hyprhands serve [--monitor NAME]     the protocol (see src/proto.rs)
ssh my-desktop hyprhands serve       the same, from another machine
```

## Takeover

The owner is at the controls when the cursor drifts more than `--tolerance`
(64) logical px from where hyprhands left it, or focus moves to another
monitor. The first sign latches and every later input is refused. Creating
`$XDG_RUNTIME_DIR/hyprhands-stop` does the same, explicitly.

## Status

Phase 1 (IPC, capture, input, takeover, config). Accessibility tree and
screen text next. See NOTICE.md for what this learned from hypruse and
typesafe-computer-use.
