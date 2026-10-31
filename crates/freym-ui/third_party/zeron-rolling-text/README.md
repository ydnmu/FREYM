# Zeron rolling text adaptation

Upstream: https://github.com/zeronsh/zeron

Revision: `dbb639be372157a66603e2f0462d3702d3d33503`

Source: `crates/ui/src/roll_text.rs`; easing/resize values in `crates/ui/src/motion.rs`.

Copyright (c) 2026 Wing. MIT license; the complete upstream text is in
`LICENSE-MIT` here and in FREYM's root `NOTICE`, which native packaging includes.

`src/ui/primitives/rolling_text.rs` adapts the character travel (0.35 em), scale
(0.6), blur (0.1 em), stagger (0.3), 550 ms roll, 200 ms width/shared-tail glide,
non-overlapping shared ends and cubic-bezier curves. FREYM uses window-scoped
keyed state, snapshots active glyph poses and width for rapid changes, reclaims
outgoing glyphs on reversal, and requests display frames only while moving.
There is no global label map, pulse clock, animation timer or dependency change.

Pinned GPUI 0.2.2 does not expose Zeron's `paint_glyph_transformed` method. The
adaptation scales through font size and approximates blur using nine weighted
glyph samples. It cannot reproduce the 2 degree composite tilt or exact Gaussian
blur; font hinting may differ while scaled. At rest it uses the normal shaped
line painter. Emoji use the platform glyph painter and switch at half opacity.
Other FREYM source retains its existing license.
