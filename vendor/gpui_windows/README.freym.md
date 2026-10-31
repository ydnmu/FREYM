# Pinned Windows renderer correction

This is `crates/gpui_windows` from Zed/GPUI revision
`eec3376016e83b79a574039fb66107d3801741f6`, under its included Apache-2.0 license.
Only this backend is patched; all other GPUI packages retain that Git revision.
The copy is approximately 0.5 MB. Its manifest has a standalone workspace with
the original inherited dependencies/lints expanded; upstream path dependencies
point to the same pinned Git revision. No dependency versions were upgraded.

The only renderer changes are in `src/directx_renderer.rs`:

- `create_blend_state`: destination alpha factor `ONE` → `INV_SRC_ALPHA`.
- `create_blend_state_for_path_sprite`: the same correction.

RGB factors, shaders, subpixel text and path rasterization are unchanged. The
frame uses a premultiplied-alpha DirectComposition swap chain. Its accumulated
alpha must be `As + Ad * (1 - As)`, consistent with its accumulated RGB. Adding
`As + Ad` saturated overlapping translucent surfaces toward opacity.

For example, a 0.3125-alpha row over a 0.66-alpha panel requires final alpha
`0.3125 + 0.66 * (1 - 0.3125) = 0.76625` (about 195/255), rather than their
sum (about 248/255). The path-rasterization blend state already uses source-over;
subpixel text deliberately excludes alpha writes.

Keep this backend aligned with the workspace GPUI pin. Before removing or
refreshing the patch, compare overlapping translucent surfaces and opaque-frame
RGBA output using the pinned DirectX backend. Render-target readback does not
establish visible desktop transparency, OS Acrylic acceptance or GPU performance;
those need separate native validation.
