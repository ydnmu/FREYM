# Building

The verified build target is Windows x64 with MSVC. Install Rust/rustup,
PowerShell 7, Visual Studio C++ Build Tools with a Windows SDK, CMake and Ninja.
The repository selects Rust 1.96.0 with rustfmt and Clippy. Python 3 is needed for
tooling tests. Put these tools on PATH. No Node, Tauri or WebView2 is required.

From the repository root in PowerShell 7, keep artifacts outside the checkout:

```powershell
$env:CARGO_TARGET_DIR = Join-Path (Split-Path (Get-Location) -Parent) 'freym-target'
$env:CARGO_BUILD_JOBS = '2'
pwsh -NoProfile -File scripts/run-task.ps1 cargo fetch --locked
pwsh -NoProfile -File scripts/run-task.ps1 cargo build --locked --release -p freym-app --bin freym -p freym-update --bin freym-launcher --jobs 2
```

The outputs are `release/freym.exe` and `release/freym-launcher.exe` beneath
`CARGO_TARGET_DIR`. This produces local binaries, not an installer or signed
release. Production updates are disabled. Binary distribution requires separate
dependency notices, license/source coverage and runtime validation; the repository
does not generate a complete distribution notice bundle.

The optional component lab uses example state and the real GPUI components:

```powershell
pwsh -NoProfile -File scripts/run-task.ps1 cargo build --locked --release -p freym-app --features ui-preview --bin freym-ui-preview --jobs 2
```

Run focused tests for changed code. Common checks are:

```powershell
pwsh -NoProfile -File scripts/run-task.ps1 cargo fmt --all --check
pwsh -NoProfile -File scripts/run-task.ps1 cargo clippy --locked --workspace --all-targets --all-features --jobs 2 -- -D warnings
pwsh -NoProfile -File scripts/run-task.ps1 cargo test --locked -p freym-update --lib --jobs 2
python -B -m unittest discover -s tests -p "test_*.py"
```

Media fixtures need FFmpeg and ffprobe on PATH. Studio export also needs those
tools. Read native tests and examples before running them: some capture a display
or audio device. Ignored hardware/fixture checks are not passing acceptance tests.
Use `--offline` only after the required dependencies are cached.

Linux native code needs GStreamer and native build dependencies. Optional
`libpulse >=13` development files enable the process-audio adapter; DMA-BUF
support additionally needs GStreamer VA 1.24 and libva 1.15. macOS native code
requires Apple SDKs. Windows checks do not validate either platform's native
application or recording behavior.

The historical intermittent Windows manifest Error 5 is not proven eliminated in
all environments. The exclusive-rename fix passed focused tests and one short SDR
video-only recording. Sustained A/V synchronization, physical HDR and power-loss
durability remain unverified.
