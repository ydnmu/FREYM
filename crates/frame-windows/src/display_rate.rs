//! Refresh-rate conversion and display-configuration association (FRAME-0102).
//!
//! A display's refresh rate is a property of its *current display mode*: the rate
//! Windows is configured to present that output at. It is not a measurement. It is
//! neither the rate at which Frame captures unique frames nor the rate at which the
//! encoder emits them, and a variable-refresh display may present frames outside
//! that nominal rate. Those are separate observations and stay `null` until their own
//! stages measure them.
//!
//! Windows exposes the active rate as a `DISPLAYCONFIG_RATIONAL` on the active
//! display-configuration path. The numerator and denominator are kept internally and
//! the canonical wire value is produced by a plain division, so a mode such as
//! 60000/1001 is not rounded to the 60 Hz an integer `DEVMODE` field would report;
//! the `f64` is the closest double to that rational, not a mathematically exact
//! representation of it.
//!
//! Everything here is crate-private: the rational, the association evidence and the
//! native query never cross the contract boundary, which stays
//! `DisplayInfo.refresh_rate_hz: Option<f64>`. The rules carry no state and make no
//! native calls, so they are covered by the unit tests in this module.
//!
//! The module is compiled for production only on Windows, where the backend consumes
//! it; on other hosts it is compiled for test builds so these platform-independent
//! rules stay covered without leaving unused code in a production build.

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};

/// The native refresh rate of one active display-configuration path.
///
/// The numerator and denominator stay internal: callers read the canonical value
/// through [`NativeRefreshRate::hz`], and the native rational is available as a
/// diagnostic string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeRefreshRate {
    numerator: u32,
    denominator: u32,
}

impl NativeRefreshRate {
    /// Builds a rate from a `DISPLAYCONFIG_RATIONAL`.
    ///
    /// An all-zero rational is treated as an unspecified rate and becomes `None`.
    /// Microsoft's "the caller does not specify a refresh rate" wording describes the
    /// caller-side contract of `SetDisplayConfig`, so an all-zero *query* result is
    /// handled by this conservative policy rather than by a documented guarantee.
    ///
    /// A half-zero rational (a zero numerator with a non-zero denominator, or the
    /// reverse) is inconsistent native data and is an explicit error, so a native
    /// failure and an unknown rate stay different outcomes.
    pub(crate) fn from_rational(
        numerator: u32,
        denominator: u32,
    ) -> Result<Option<Self>, RecorderError> {
        match (numerator, denominator) {
            (0, 0) => Ok(None),
            (0, _) => Err(rate_error(format!(
                "Windows reported a zero refresh-rate numerator with denominator {denominator}."
            ))),
            (_, 0) => Err(rate_error(format!(
                "Windows reported a zero refresh-rate denominator with numerator {numerator}."
            ))),
            _ => Ok(Some(Self {
                numerator,
                denominator,
            })),
        }
    }

    /// The rate in hertz, as carried by the canonical wire contract.
    pub(crate) fn hz(&self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }
}

impl std::fmt::Display for NativeRefreshRate {
    /// Prints the native rational, for example `60000/1001`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.numerator, self.denominator)
    }
}

/// One active display-configuration path, reduced to plain values.
///
/// The source output name, target monitor device path, friendly name, output
/// technology and status flags are association evidence: they let a reported rate be
/// traced to the path and monitor it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveRateRecord {
    /// Source output name, for example `\\.\DISPLAY1`.
    pub(crate) source_gdi_device_name: String,
    /// Target monitor device-interface path: the same stable identity that
    /// `DisplayInfo.id` carries.
    pub(crate) target_monitor_device_path: String,
    /// Monitor name reported by the display configuration. Evidence only.
    pub(crate) target_friendly_name: String,
    /// Native rate, or `None` when the path's rational was all zero.
    pub(crate) refresh: Option<NativeRefreshRate>,
    /// `DISPLAYCONFIG_PATH_TARGET_INFO.targetAvailable`.
    pub(crate) target_available: bool,
    /// `DISPLAYCONFIG_PATH_TARGET_INFO.outputTechnology`.
    pub(crate) output_technology: u32,
    /// `DISPLAYCONFIG_PATH_TARGET_INFO.statusFlags`.
    pub(crate) status_flags: u32,
}

/// Finds the active path that belongs to one enumerated display.
///
/// The whole record is returned, so a display's rate and the evidence reported beside
/// it come from one lookup of one observation. Association requires two documented
/// matches: the display-configuration target monitor device path must equal the
/// display's stable identity, and the source output name must equal the adapter
/// output the monitor was enumerated under. Both comparisons ignore case because
/// Windows device and output names are case-insensitive. Anything other than exactly
/// one matching path is rejected, so no other monitor's rate can be selected, and a
/// target that is no longer available is an explicit error.
pub(crate) fn find_active_rate<'a>(
    records: &'a [ActiveRateRecord],
    display_id: &str,
    adapter_name: &str,
) -> Result<&'a ActiveRateRecord, RecorderError> {
    let matching: Vec<&ActiveRateRecord> = records
        .iter()
        .filter(|record| {
            record
                .target_monitor_device_path
                .eq_ignore_ascii_case(display_id)
        })
        .collect();

    let record = match matching.as_slice() {
        [] => {
            return Err(rate_error(format!(
                "the active display configuration contains no path for display {display_id}, so its refresh rate is not known."
            )));
        }
        [only] => *only,
        several => {
            return Err(rate_error(format!(
                "the active display configuration contains {} paths for display {display_id}; the refresh-rate association is ambiguous.",
                several.len()
            )));
        }
    };

    if !record
        .source_gdi_device_name
        .eq_ignore_ascii_case(adapter_name)
    {
        return Err(rate_error(format!(
            "display {display_id} is published by output {} instead of the enumerated output {adapter_name}, so its refresh rate is not known.",
            record.source_gdi_device_name
        )));
    }
    if !record.target_available {
        return Err(rate_error(format!(
            "display {display_id} is on an active display-configuration path whose target is no longer available, which is the documented transient state of a display being removed."
        )));
    }

    Ok(record)
}

/// A refresh-rate observation failed: the enumeration itself is outside recording,
/// so no recovery status is claimed. The code matches the display-source taxonomy
/// used by FRAME-0101, because the unusable data is display data.
fn rate_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Retry enumeration; if it persists, report the Windows build and display adapter."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(numerator: u32, denominator: u32) -> NativeRefreshRate {
        NativeRefreshRate::from_rational(numerator, denominator)
            .expect("a valid rational must convert")
            .expect("a valid rational must not be unknown")
    }

    fn record(path: &str, source: &str, refresh: Option<NativeRefreshRate>) -> ActiveRateRecord {
        ActiveRateRecord {
            source_gdi_device_name: source.to_string(),
            target_monitor_device_path: path.to_string(),
            target_friendly_name: "Synthetic Panel".to_string(),
            refresh,
            target_available: true,
            output_technology: 11,
            status_flags: 1,
        }
    }

    const PATH_INTERNAL: &str =
        "\\\\?\\DISPLAY#BOE0D5E#5&12ac677&0&UID8448#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
    const PATH_EXTERNAL: &str =
        "\\\\?\\DISPLAY#DEL4098#5&2b1a3c4d&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";

    #[test]
    fn integer_rates_convert_without_rounding() {
        for hertz in [60u32, 144, 165, 240, 480, 600] {
            let converted = rate(hertz, 1);
            assert_eq!(converted.hz(), f64::from(hertz), "{hertz} Hz");
            assert_eq!(converted.to_string(), format!("{hertz}/1"));
        }
    }

    #[test]
    fn fractional_rates_are_not_rounded_to_integer_hertz() {
        // The classic NTSC-derived rates an integer DEVMODE field reports as 60.
        let ntsc = rate(60_000, 1_001);
        assert_eq!(ntsc.to_string(), "60000/1001");
        assert_ne!(ntsc.hz(), 60.0);
        // The wire value is a plain division of the native rational, so it is the
        // closest double and round-trips within f64 precision.
        assert!((ntsc.hz() * 1_001.0 - 60_000.0).abs() < 1e-9);

        for (numerator, denominator, expected) in [
            (30_000u32, 1_001u32, 29.970_029_970_029_97f64),
            (24_000, 1_001, 23.976_023_976_023_976),
            (119_880, 1_001, 119.760_239_760_239_76),
        ] {
            let converted = rate(numerator, denominator);
            assert_eq!(converted.to_string(), format!("{numerator}/{denominator}"));
            assert!(
                (converted.hz() - expected).abs() < 1e-12,
                "{numerator}/{denominator} produced {}",
                converted.hz()
            );
        }
    }

    #[test]
    fn synthetic_rates_above_a_thousand_hertz_are_not_rejected() {
        // Synthetic conversion coverage only: no such mode was tested on hardware,
        // and no upper bound is imposed on a reported rate.
        let high = rate(1_440, 1);
        assert_eq!(high.hz(), 1_440.0);
        assert!(high.hz() > 1_000.0);

        let rescaled = rate(120_000, 100);
        assert_eq!(rescaled.hz(), 1_200.0);
    }

    #[test]
    fn all_zero_is_unspecified_and_half_zero_is_an_error() {
        // The all-zero policy is conservative, not a documented QueryDisplayConfig
        // guarantee: it must not become a number, and it is not a native failure.
        assert_eq!(
            NativeRefreshRate::from_rational(0, 0)
                .expect("an all-zero rational is not a native failure"),
            None
        );

        for (numerator, denominator, fragment) in [
            (0u32, 1u32, "zero refresh-rate numerator"),
            (60, 0, "zero refresh-rate denominator"),
            (1, 0, "zero refresh-rate denominator"),
        ] {
            let error = NativeRefreshRate::from_rational(numerator, denominator)
                .expect_err("an inconsistent rational must be an explicit error");
            assert_eq!(error.code, ErrorCode::SourceLost);
            assert_eq!(error.recovery, RecoveryStatus::NoRecording);
            assert!(
                error.message.contains(fragment),
                "unexpected message: {}",
                error.message
            );
        }
    }

    #[test]
    fn association_matches_identity_and_output_case_insensitively() {
        let records = vec![record(PATH_INTERNAL, "\\\\.\\DISPLAY1", Some(rate(144, 1)))];

        let matched = find_active_rate(&records, PATH_INTERNAL, "\\\\.\\DISPLAY1")
            .expect("the active path must be found");
        assert_eq!(matched.refresh.map(|rate| rate.hz()), Some(144.0));
        // The whole record comes back, so its evidence belongs to this lookup.
        assert_eq!(matched.source_gdi_device_name, "\\\\.\\DISPLAY1");
        assert_eq!(matched.target_monitor_device_path, PATH_INTERNAL);
        assert_eq!(matched.target_friendly_name, "Synthetic Panel");

        let lowercased = PATH_INTERNAL.to_ascii_lowercase();
        let matched = find_active_rate(&records, &lowercased, "\\\\.\\display1")
            .expect("case must not change association");
        assert_eq!(matched.refresh.map(|rate| rate.hz()), Some(144.0));
    }

    #[test]
    fn each_display_receives_only_its_own_evidence() {
        let records = vec![
            record(PATH_INTERNAL, "\\\\.\\DISPLAY1", Some(rate(165, 1))),
            record(PATH_EXTERNAL, "\\\\.\\DISPLAY2", Some(rate(60_000, 1_001))),
        ];

        let internal = find_active_rate(&records, PATH_INTERNAL, "\\\\.\\DISPLAY1")
            .expect("internal display must match");
        assert_eq!(internal.refresh.unwrap().to_string(), "165/1");
        assert_eq!(internal.target_monitor_device_path, PATH_INTERNAL);

        let external = find_active_rate(&records, PATH_EXTERNAL, "\\\\.\\DISPLAY2")
            .expect("external display must match");
        assert_eq!(external.refresh.unwrap().to_string(), "60000/1001");
        assert_eq!(external.target_monitor_device_path, PATH_EXTERNAL);

        // The reverse pairing must not resolve, because the output does not match.
        assert!(find_active_rate(&records, PATH_INTERNAL, "\\\\.\\DISPLAY2").is_err());
        assert!(find_active_rate(&records, PATH_EXTERNAL, "\\\\.\\DISPLAY1").is_err());
    }

    #[test]
    fn absent_ambiguous_and_mismatched_associations_are_rejected() {
        let absent = find_active_rate(
            &[record(PATH_INTERNAL, "\\\\.\\DISPLAY1", Some(rate(144, 1)))],
            PATH_EXTERNAL,
            "\\\\.\\DISPLAY2",
        )
        .expect_err("an absent display must not borrow another monitor's rate");
        assert!(absent.message.contains("no path for display"));

        let wrong_output = find_active_rate(
            &[record(PATH_INTERNAL, "\\\\.\\DISPLAY2", Some(rate(144, 1)))],
            PATH_INTERNAL,
            "\\\\.\\DISPLAY1",
        )
        .expect_err("a different output must not supply the rate");
        assert!(wrong_output.message.contains("published by output"));

        let ambiguous = find_active_rate(
            &[
                record(PATH_INTERNAL, "\\\\.\\DISPLAY1", Some(rate(144, 1))),
                record(PATH_INTERNAL, "\\\\.\\DISPLAY1", Some(rate(60, 1))),
            ],
            PATH_INTERNAL,
            "\\\\.\\DISPLAY1",
        )
        .expect_err("two paths for one display are ambiguous");
        assert!(ambiguous.message.contains("ambiguous"));
    }

    #[test]
    fn unplugged_targets_and_unspecified_rates_stay_distinct() {
        let mut unplugged = record(PATH_INTERNAL, "\\\\.\\DISPLAY1", Some(rate(144, 1)));
        unplugged.target_available = false;
        let error = find_active_rate(&[unplugged], PATH_INTERNAL, "\\\\.\\DISPLAY1")
            .expect_err("a target that is no longer available must be an explicit error");
        assert!(error.message.contains("no longer available"));

        let unspecified = [record(PATH_INTERNAL, "\\\\.\\DISPLAY1", None)];
        let matched = find_active_rate(&unspecified, PATH_INTERNAL, "\\\\.\\DISPLAY1")
            .expect("a valid path with an unspecified rate is not a native failure");
        assert!(
            matched.refresh.is_none(),
            "an unspecified rate must stay unknown instead of becoming a number"
        );
    }
}
