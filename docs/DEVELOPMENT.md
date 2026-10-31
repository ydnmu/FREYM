# Development

Use the pinned Rust 1.96.0 toolchain, MSVC, Windows SDK, CMake and PowerShell 7.
Run Cargo sequentially with at most two jobs. On Windows use the owned runner:

```powershell
pwsh -NoProfile -File scripts/run-task.ps1 cargo build --locked --release -p freym-app --bin freym -p freym-update --bin freym-launcher --jobs 2
```

Native fonts are checked in; regeneration uses the bundled OFL sources with
fonttools 4.60.1 and brotli 1.1.0 via scripts/prepare-ui-lab-fonts.py.
Core headless tests live alongside the crates. Platform/device tests require
explicit scope. See RELEASE.md for packaging.
