//! Diagnostic Windows display capture probe (FRAME-0103).
//!
//! Runs the production capture session for a bounded time and prints what it
//! observed. It is read-only: no file, screenshot or recording is written, no display
//! mode is changed and no input is sent to any application. This is a proof of the
//! capture path, not a recorder and not evidence of encoding, A/V sync or HDR.
//!
//! ```text
//! cargo run --locked -p frame-windows --example capture_probe -- \
//!     --display-id "\\?\DISPLAY#BOE0D5E#5&12ac677&0&UID8448#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}" \
//!     --seconds 30
//! ```
//!
//! Options: `--display-id ID` (required), `--seconds N` (1..=3600, default 30),
//! `--stall-consumer-ms N` (hold every lease for N ms, at most the duration) and
//! `--verify-pixels` (opt-in bounded readback verification; it adds copies and is
//! reported separately).

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    probe::run()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("Native display capture is implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod probe {
    use frame_contracts::DisplayId;
    use std::process::ExitCode;

    const USAGE: &str = "usage: capture_probe --display-id ID [--seconds 1..3600] [--stall-consumer-ms N] [--verify-pixels]";

    struct Options {
        display_id: Option<String>,
        seconds: u32,
        stall_consumer_ms: u32,
        verify_pixels: bool,
    }

    fn parse_options() -> Result<Options, String> {
        let mut options = Options {
            display_id: None,
            seconds: 30,
            stall_consumer_ms: 0,
            verify_pixels: false,
        };
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--display-id" => {
                    options.display_id = Some(
                        arguments
                            .next()
                            .ok_or_else(|| "--display-id needs a value".to_string())?,
                    );
                }
                "--seconds" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--seconds needs a value".to_string())?;
                    options.seconds = value
                        .parse()
                        .map_err(|_| format!("--seconds expects a number, not {value}"))?;
                }
                "--stall-consumer-ms" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--stall-consumer-ms needs a value".to_string())?;
                    options.stall_consumer_ms = value.parse().map_err(|_| {
                        format!("--stall-consumer-ms expects a number, not {value}")
                    })?;
                }
                "--verify-pixels" => options.verify_pixels = true,
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(options)
    }

    /// Lists the enumerated displays so an operator copies a real identity instead of
    /// guessing one.
    fn available_displays() -> String {
        use frame_capture::DisplayBackend;
        match frame_windows::PlatformBackend.enumerate_displays() {
            Ok(displays) => displays
                .iter()
                .map(|display| {
                    format!(
                        "  {} — {}x{} primary={} refresh_rate_hz={:?}\n",
                        display.id.0,
                        display.width,
                        display.height,
                        display.is_primary,
                        display.refresh_rate_hz
                    )
                })
                .collect(),
            Err(error) => format!("  (enumeration failed: {})\n", error.message),
        }
    }

    pub(super) fn run() -> ExitCode {
        let options = match parse_options() {
            Ok(options) => options,
            Err(message) => {
                eprintln!("capture_probe: {message}");
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        };

        let Some(display_id) = options.display_id.as_ref() else {
            eprintln!(
                "capture_probe: --display-id is required; use an id from a real display, never a guess."
            );
            eprintln!("Currently enumerated displays:");
            eprint!("{}", available_displays());
            return ExitCode::FAILURE;
        };

        match frame_windows::capture_probe(
            &DisplayId(display_id.clone()),
            options.seconds,
            options.stall_consumer_ms,
            options.verify_pixels,
        ) {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                // The probe embeds its partial report in the message, so a failed run
                // still shows what was observed before the failure.
                eprintln!("capture_probe failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                ExitCode::FAILURE
            }
        }
    }
}
