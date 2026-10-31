# Development rules

Read [development](docs/DEVELOPMENT.md) and [architecture](docs/ARCHITECTURE.md)
before editing. Preserve unrelated changes and user files. Keep changes bounded;
remove code only after checking callers, feature combinations and packaging.

- FREYM is native Rust/GPUI. `freym-app` and `freym-ui` own presentation;
  `frame-runtime::RecordingControl` owns recording coordination. Reuse the shared
  contracts and components instead of creating parallel implementations.
- Preserve capture, preview, encoder, audio, MKV, recovery and permission behavior.
  Keep unsafe operations in platform boundaries, with explicit resource/thread
  ownership, bounded queues and unsupported-operation errors.
- Run Cargo commands sequentially with at most two jobs. On Windows use
  `scripts/run-task.ps1` so child processes remain owned. Use the headless checks
  in the development guide; GUI, recording and hardware tests require a specific
  scope. Fixtures do not establish device or release acceptance.
- Fix failing tests and warnings at their cause. Do not hide failures with ignored
  tests, weaker assertions or blanket lint suppression. Report unverified areas.
- Preserve LICENSE, NOTICE, third-party attribution and source/distribution gates.
  Do not add assistant, model or provider attribution to source or Git metadata.
- Keep documentation current and concise. Task logs, generated audits, private
  backups, credentials, binaries and recordings belong outside the source tree.
- Publishing, destructive Git operations and repository administration require
  explicit maintainer authorization. A build does not authorize replacing an
  installed application or publishing a release.
