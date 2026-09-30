# Notices

hyprhands is original Rust, but it learned from two MIT-licensed projects and
ports ideas and specifics from them. Their copyright notices are reproduced
here as the MIT license requires.

## hypruse

<https://github.com/IlyasKhallouki/hypruse>, studied at commit
`9a84bdb10fe755527b77fcc388d9a452526b9c5f`.

Copyright (c) 2026 Ilyas Khallouki. MIT License.

Taken from it:

- Positioning the cursor with Hyprland's `movecursor` dispatcher and sending
  only buttons and scroll on the virtual pointer (absolute virtual-pointer
  motion misbehaves across monitors, hyprwm/Hyprland#6749).
- Scroll as `axis_discrete` with 15 axis units per notch.
- Typing through a generated XKB keymap (`U<hex>` keysyms, one key per
  character, after wtype), and combos as real modifier keys with a
  `modifier_map`: layout-independent and unicode-correct.
- The seat-contention idea behind takeover detection (its `HYPRUSE_STRICT`).

## typesafe-computer-use (jev)

<https://github.com/awlevin/typesafe-computer-use>, studied at commit
`49d2b5a3980d70fd66e6a0794c786f8eb8d2300e`.

Copyright (c) 2026 Aaron Levin. MIT License.

Taken from it:

- The observation shape an agent loop needs: one display in logical points,
  the frontmost window, the focused field, labelled controls.
- The OSWorld-shaped accessibility XML its `osworld/a11y.py` reads (role tags,
  `st:`/`cp:`/`act:`/`attr:` namespaces), which hyprhands' tree emits so that
  reader works unchanged.

The tree walk (`src/a11y.rs`: showing-only descent, node and time caps, the
`button` → `push-button` alias) and the screen-text rules (`src/screentext.rs`:
deepest text only, viewport clipping, covered and visually-hidden text dropped)
port the Hyprland adapter in the MasonRhodesDev fork of that repository
(`typesafe_computer_use/hyprland/remote.py` and `tree_text.py` at `48fc540`),
which is this repository's own author's work on top of it.

Full license text of both: see each project's LICENSE at the commits above;
both are the standard MIT text also in this repository's LICENSE.
