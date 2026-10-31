//! Read-only native display probe (FRAME-0101, FRAME-0102).
//!
//! ```text
//! cargo run --locked -p frame-windows --example list_displays
//! ```
//!
//! It prints the human-readable diagnostics report for this machine, so the output can
//! be compared with Windows display settings. The canonical values and the rate
//! evidence beside them come from one enumeration observation. It reads no pixels,
//! writes no files and enables no recording capability.
//!
//! A refresh value in the report is the nominal rate of the current display mode. It is
//! not a capture rate, not an encode rate and not a measurement of delivered frames.

#[cfg(target_os = "windows")]
fn main() {
    match frame_windows::display_diagnostics() {
        Ok(report) => print!("{report}"),
        Err(error) => {
            eprintln!("Display diagnostics failed: {}", error.message);
            eprintln!("code: {:?}  recovery: {:?}", error.code, error.recovery);
            eprintln!("action: {}", error.action);
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Display enumeration is implemented for Windows only.");
    std::process::exit(1);
}
