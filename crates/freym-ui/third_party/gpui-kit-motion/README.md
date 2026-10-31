# GPUI Kit motion adaptation

Upstream: https://github.com/longbridge/gpui-kit

Revision: `7b14e03f2ed4b46fc4149a8a523c7f91bcad6560`

Source: `crates/base/src/motion.rs`, `Spring::config`.

`response_spring` in `src/ui/theme/motion.rs` adapts that frequency/configuration
calculation. Changes: accepts milliseconds, clamps the minimum response to 16 ms,
and fixes the damping ratio at 1 for a non-bouncing control response. No dependency
or full motion framework was imported.

Copyright 2024 - 2026 Longbridge <https://longbridge.com>

License: Apache-2.0; the exact upstream license is in `LICENSE-APACHE` alongside
this file. Other FREYM code retains its existing license.
