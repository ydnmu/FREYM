# Contributing

Keep changes focused on a demonstrated problem. Preserve settings compatibility,
native resource ownership, bounded queues and explicit failure reporting. Plugin
runtimes and marketplace implementations are outside Core.

Use the pinned toolchain and lockfile. Follow [BUILDING.md](BUILDING.md), run checks
relevant to the change and report failures, ignored tests and platform limits.
Review concurrency and failure paths independently before integration. Do not
treat compilation or synthetic tests as recording acceptance.

Retain copyright notices and document the source and license of imported code or
assets. Contributions are provided under GPL-3.0-or-later unless an existing
third-party license applies. Do not include recordings, credentials, local build
outputs or unrelated formatting changes. Report vulnerabilities as described in
[SECURITY.md](SECURITY.md).
