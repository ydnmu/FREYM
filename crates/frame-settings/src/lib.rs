//! Pure request validation, separate from OS capability/permission validation.
//!
//! It also owns the canonical region coordinate rules (FRAME-0205). A region is expressed
//! in the **selected display's physical pixel space**: the origin is that display's own
//! top-left corner, `x` grows to the right, `y` grows down, and the rectangle is half-open
//! (`[x, x + width)` by `[y, y + height)`). It is therefore neither a global desktop
//! rectangle — which is negative for a display left of or above the primary — nor a
//! DPI-scaled CSS selection. Every conversion here produces canonical coordinates or an
//! explicit error; none of them clamps, rescales or silently drops part of a selection.
//!
//! Two further concerns live beside it: the recording policy that turns the canonical
//! selections plus reported hardware values into one explicit plan
//! ([`policy`]), and the versioned local settings document ([`persistence`]).
pub mod assessment;
pub mod capabilities;
pub mod model;
pub mod persistence;
pub mod policy;
pub mod validation;
pub use persistence::{
    SETTINGS_FILE_NAME, SETTINGS_SCHEMA_VERSION, SettingsError, SettingsErrorKind, SettingsStore,
};
pub use policy::{
    DecisionCode, EncoderCandidate, EncoderLimits, EncoderPreset, FrameRatePlan, H264Level,
    NominalRate, PlanDecision, PlanInputs, RateControl, RecordingPlan, ResolutionPlan,
    SelectedEncoder, SessionPlan, apply_bitrate_ceiling, documented_bitrate_bps,
    keyframe_interval_frames, plan_recording, plan_recording_for_codec, required_level,
    vbr_peak_bps,
};

use frame_contracts::{
    CaptureTarget, DisplayInfo, EncoderPreference, FpsMode, RecorderError, RecordingRequest,
    Region, Resolution,
};
use std::collections::BTreeSet;

pub fn validate_request(request: &RecordingRequest) -> Result<(), RecorderError> {
    if let Some(o) = &request.optimization {
        o.encoder_overrides.validate()?;
        if !o.encoder_overrides.is_empty() && o.preset == frame_contracts::RecordingPreset::Lossless
        {
            return Err(RecorderError::invalid(
                "Clear native encoder overrides before selecting the lossless profile.",
            ));
        }
        if o.encoder_overrides.rate_control == Some(frame_contracts::VideoRateControl::Cqp)
            && o.bitrate_bps.is_some()
        {
            return Err(RecorderError::invalid(
                "CQP does not use a bitrate target. Clear the bitrate override.",
            ));
        }
        #[cfg(not(target_os = "windows"))]
        if !o.encoder_overrides.is_empty() {
            return Err(RecorderError::invalid(
                "Native Advanced overrides are currently implemented only on Windows NVENC.",
            ));
        }
    }
    if request
        .optimization
        .as_ref()
        .is_some_and(|o| o.bitrate_bps == Some(0))
    {
        return Err(RecorderError::invalid("Bitrate must be greater than zero."));
    }
    if let FpsMode::Fixed(fps) = request.fps_mode
        && (!fps.is_finite() || fps <= 0.0)
    {
        return Err(RecorderError::invalid(
            "Frame rate must be finite and greater than zero.",
        ));
    }
    match &request.target {
        CaptureTarget::Display(id) if id.0.trim().is_empty() => {
            return Err(RecorderError::invalid("Choose a display."));
        }
        CaptureTarget::Window(id) if id.0.trim().is_empty() => {
            return Err(RecorderError::invalid("Choose a window."));
        }
        CaptureTarget::Region(region)
            if region.width == 0 || region.height == 0 || region.display_id.0.trim().is_empty() =>
        {
            return Err(RecorderError::invalid("Choose a nonempty display region."));
        }
        CaptureTarget::Game(game) if game.process_id == 0 => {
            return Err(RecorderError::invalid("Choose a game process."));
        }
        _ => {}
    }
    if let Resolution::Exact(size) = request.resolution
        && (size.width == 0 || size.height == 0)
    {
        return Err(RecorderError::invalid("Resolution must be nonzero."));
    }
    if let EncoderPreference::Explicit(id) = &request.encoder
        && id.trim().is_empty()
    {
        return Err(RecorderError::invalid("Choose an encoder."));
    }
    let stem = &request.output.file_stem;
    if request.output.directory.trim().is_empty()
        || stem.trim().is_empty()
        || stem == "."
        || stem == ".."
        || stem
            .chars()
            .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
    {
        return Err(RecorderError::invalid(
            "Choose an output directory and a plain file name.",
        ));
    }
    let mut ids = BTreeSet::new();
    for route in &request.audio_routes {
        route.controls.validate()?;
        if route.source_id.0.trim().is_empty() || !ids.insert(&route.source_id) {
            return Err(RecorderError::invalid(
                "Audio source IDs must be nonempty and unique.",
            ));
        }
    }
    Ok(())
}

// ------------------------------------------------------------ canonical regions

/// A half-open rectangle in one display's physical pixel space.
///
/// `right` and `bottom` are exclusive, so `width()` is `right - left` and a rectangle that
/// touches the display's right or bottom edge exactly is inside it.
///
/// Values are produced only by the conversions in this module, which reject an empty,
/// negative, overflowing or out-of-display rectangle instead of clamping it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionBounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl RegionBounds {
    pub fn left(&self) -> i32 {
        self.left
    }

    pub fn top(&self) -> i32 {
        self.top
    }

    /// Exclusive right edge.
    pub fn right(&self) -> i32 {
        self.right
    }

    /// Exclusive bottom edge.
    pub fn bottom(&self) -> i32 {
        self.bottom
    }

    pub fn width(&self) -> u32 {
        u32::try_from(self.right - self.left).unwrap_or(0)
    }

    pub fn height(&self) -> u32 {
        u32::try_from(self.bottom - self.top).unwrap_or(0)
    }
}

/// The display's whole physical surface as a canonical half-open rectangle.
///
/// A display that reports no pixels is an explicit error: an empty display cannot be a
/// capture target and is never replaced by a default geometry.
pub fn display_pixel_bounds(display: &DisplayInfo) -> Result<RegionBounds, RecorderError> {
    if display.width == 0 || display.height == 0 {
        return Err(RecorderError::invalid(format!(
            "The display {} reports no physical pixels ({}x{}), so it has no region space to select in.",
            display.id.0, display.width, display.height
        )));
    }
    let right = i32::try_from(display.width).map_err(|_| {
        RecorderError::invalid(format!(
            "The display {} reports a width of {} px, which does not fit the region coordinate space.",
            display.id.0, display.width
        ))
    })?;
    let bottom = i32::try_from(display.height).map_err(|_| {
        RecorderError::invalid(format!(
            "The display {} reports a height of {} px, which does not fit the region coordinate space.",
            display.id.0, display.height
        ))
    })?;
    Ok(RegionBounds {
        left: 0,
        top: 0,
        right,
        bottom,
    })
}

/// Checks one canonical region against the display it names and returns its bounds.
///
/// Everything that cannot become a real crop is refused here, before any capture session
/// exists: a region for another display, an empty rectangle, a rectangle that leaves the
/// display in any direction, and dimensions the accepted H.264 4:2:0 encoder path cannot
/// encode. The rectangle is never trimmed, moved or re-scaled to fit.
pub fn validate_region(
    region: &Region,
    display: &DisplayInfo,
) -> Result<RegionBounds, RecorderError> {
    if !region.display_id.0.eq_ignore_ascii_case(&display.id.0) {
        return Err(RecorderError::invalid(format!(
            "The region belongs to display {} but display {} was selected; a region is only valid in the physical pixel space of the display it names.",
            region.display_id.0, display.id.0
        )));
    }
    let display_bounds = display_pixel_bounds(display)?;
    if region.width == 0 || region.height == 0 {
        return Err(RecorderError::invalid(format!(
            "The region at ({}, {}) is {}x{}: an empty selection is not a capture target.",
            region.x, region.y, region.width, region.height
        )));
    }
    if region.x < 0 || region.y < 0 {
        return Err(RecorderError::invalid(format!(
            "The region starts at ({}, {}), outside the display {} whose physical pixel space begins at (0, 0).",
            region.x, region.y, display.id.0
        )));
    }
    let right = i32::try_from(region.width)
        .ok()
        .and_then(|width| region.x.checked_add(width));
    let bottom = i32::try_from(region.height)
        .ok()
        .and_then(|height| region.y.checked_add(height));
    let (Some(right), Some(bottom)) = (right, bottom) else {
        return Err(RecorderError::invalid(format!(
            "The region at ({}, {}) with {}x{} px overflows the region coordinate space.",
            region.x, region.y, region.width, region.height
        )));
    };
    if right > display_bounds.right() || bottom > display_bounds.bottom() {
        return Err(RecorderError::invalid(format!(
            "The region ({}, {}) to ({}, {}) is outside the display {} of {}x{} px; Frame does not clamp or shrink a region to fit.",
            region.x, region.y, right, bottom, display.id.0, display.width, display.height
        )));
    }
    if !region.width.is_multiple_of(2) || !region.height.is_multiple_of(2) {
        return Err(RecorderError::invalid(format!(
            "A {}x{} region cannot be encoded by the accepted H.264 8-bit 4:2:0 path, which needs even dimensions; Frame does not pad or scale a region to make it encodable.",
            region.width, region.height
        )));
    }
    Ok(RegionBounds {
        left: region.x,
        top: region.y,
        right,
        bottom,
    })
}

/// Converts a rectangle in global desktop coordinates into a canonical region.
///
/// The desktop origin can be negative — a display left of or above the primary has a
/// negative desktop origin — so the conversion subtracts that display's own desktop
/// origin and then validates the result. A rectangle that is not wholly inside the display
/// is an explicit error, not a partially captured recording.
pub fn region_from_desktop_rect(
    display: &DisplayInfo,
    rect: (i32, i32, i32, i32),
) -> Result<Region, RecorderError> {
    let (left, top, right, bottom) = rect;
    if right <= left || bottom <= top {
        return Err(RecorderError::invalid(format!(
            "The desktop rectangle ({left}, {top}) to ({right}, {bottom}) is empty; a region needs a positive width and height."
        )));
    }
    let x = left.checked_sub(display.x).ok_or_else(|| {
        RecorderError::invalid(format!(
            "The desktop rectangle at ({left}, {top}) cannot be expressed relative to the display {} at ({}, {}).",
            display.id.0, display.x, display.y
        ))
    })?;
    let y = top.checked_sub(display.y).ok_or_else(|| {
        RecorderError::invalid(format!(
            "The desktop rectangle at ({left}, {top}) cannot be expressed relative to the display {} at ({}, {}).",
            display.id.0, display.x, display.y
        ))
    })?;
    let width = u32::try_from(right - left).map_err(|_| {
        RecorderError::invalid(format!(
            "The desktop rectangle width {} does not fit a region width.",
            right - left
        ))
    })?;
    let height = u32::try_from(bottom - top).map_err(|_| {
        RecorderError::invalid(format!(
            "The desktop rectangle height {} does not fit a region height.",
            bottom - top
        ))
    })?;
    let region = Region {
        display_id: display.id.clone(),
        x,
        y,
        width,
        height,
    };
    validate_region(&region, display)?;
    Ok(region)
}

/// Converts a CSS-pixel selection on `display` into a canonical physical-pixel region.
///
/// A window shows the display at the display's own scale factor, so a selection is scaled
/// by `display.scale_factor` and each edge is rounded to the nearest physical pixel. The
/// rounded rectangle is then validated against the display; a selection that leaves the
/// display after rounding is refused rather than pulled back inside it.
pub fn region_from_css_selection(
    display: &DisplayInfo,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<Region, RecorderError> {
    let scale = display.scale_factor;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(RecorderError::invalid(format!(
            "The display {} reports a scale factor of {scale}, so a CSS selection cannot be converted to physical pixels.",
            display.id.0
        )));
    }
    if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
        return Err(RecorderError::invalid(
            "A CSS selection must be finite; a NaN or infinite value cannot name a region."
                .to_string(),
        ));
    }
    if x < 0.0 || y < 0.0 {
        return Err(RecorderError::invalid(format!(
            "The CSS selection starts at ({x}, {y}), outside the display {} whose selection space begins at (0, 0).",
            display.id.0
        )));
    }
    if width <= 0.0 || height <= 0.0 {
        return Err(RecorderError::invalid(format!(
            "The CSS selection is {width}x{height}: an empty selection is not a capture target."
        )));
    }
    let left = round_to_physical(x, scale, "left", display)?;
    let top = round_to_physical(y, scale, "top", display)?;
    let right = round_to_physical(x + width, scale, "right", display)?;
    let bottom = round_to_physical(y + height, scale, "bottom", display)?;
    if right <= left || bottom <= top {
        return Err(RecorderError::invalid(format!(
            "The CSS selection ({x}, {y}) with {width}x{height} rounds to an empty physical rectangle on display {} at scale {scale}.",
            display.id.0
        )));
    }
    let region = Region {
        display_id: display.id.clone(),
        x: left,
        y: top,
        width: u32::try_from(right - left).map_err(|_| {
            RecorderError::invalid(
                "The rounded CSS selection width does not fit a region width.".to_string(),
            )
        })?,
        height: u32::try_from(bottom - top).map_err(|_| {
            RecorderError::invalid(
                "The rounded CSS selection height does not fit a region height.".to_string(),
            )
        })?,
    };
    validate_region(&region, display)?;
    Ok(region)
}

/// Rounds one CSS-pixel edge to the nearest physical pixel of `display`.
fn round_to_physical(
    value: f64,
    scale: f64,
    edge: &str,
    display: &DisplayInfo,
) -> Result<i32, RecorderError> {
    let physical = (value * scale).round();
    if physical < 0.0 || physical > f64::from(i32::MAX) {
        return Err(RecorderError::invalid(format!(
            "The CSS selection {edge} edge at {value} px scales to {physical} physical px on display {}, which is outside the region coordinate space.",
            display.id.0
        )));
    }
    Ok(physical as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_contracts::*;
    fn request() -> RecordingRequest {
        RecordingRequest {
            target: CaptureTarget::Display(DisplayId("display-1".into())),
            fps_mode: FpsMode::Fixed(960.0),
            resolution: Resolution::Native,
            quality: Quality::High,
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_routes: vec![],
            camera: None,
            show_cursor: true,
            output: OutputSettings {
                directory: "recordings".into(),
                file_stem: "test".into(),
            },
        }
    }
    #[test]
    fn high_fps_has_no_artificial_ceiling() {
        assert!(validate_request(&request()).is_ok());
    }
    #[test]
    fn rejects_nonfinite_fps_and_bad_dimensions() {
        for fps in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut r = request();
            r.fps_mode = FpsMode::Fixed(fps);
            assert!(validate_request(&r).is_err());
        }
        let mut r = request();
        r.resolution = Resolution::Exact(Dimensions {
            width: 0,
            height: 1080,
        });
        assert!(validate_request(&r).is_err());
    }
    #[test]
    fn rejects_path_in_filename() {
        let mut r = request();
        r.output.file_stem = "../escape".into();
        assert!(validate_request(&r).is_err());
    }

    fn display(id: &str, x: i32, y: i32, width: u32, height: u32, scale: f64) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.into()),
            name: "test panel".into(),
            width,
            height,
            x,
            y,
            scale_factor: scale,
            refresh_rate_hz: Some(60.0),
            is_primary: false,
            hdr_available: false,
            required_recording_quality: None,
        }
    }

    fn canonical(display: &DisplayInfo, x: i32, y: i32, width: u32, height: u32) -> Region {
        Region {
            display_id: display.id.clone(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_negative_desktop_origin_becomes_the_displays_own_top_left() {
        // A secondary display left of the primary has a negative desktop origin: the
        // canonical space still starts at that display's own (0, 0).
        let secondary = display("display-2", -1920, 200, 1920, 1080, 1.0);
        let region = region_from_desktop_rect(&secondary, (-1800, 300, -1400, 700))
            .expect("a rectangle inside the secondary display");
        assert_eq!(region.x, 120);
        assert_eq!(region.y, 100);
        assert_eq!(region.width, 400);
        assert_eq!(region.height, 400);
        assert_eq!(region.display_id.0, "display-2");
        let bounds = validate_region(&region, &secondary).expect("bounds");
        assert_eq!(
            (
                bounds.left(),
                bounds.top(),
                bounds.right(),
                bounds.bottom(),
                bounds.width(),
                bounds.height()
            ),
            (120, 100, 520, 500, 400, 400)
        );
    }

    #[test]
    fn a_css_selection_is_scaled_to_physical_pixels() {
        // A 2560x1600 panel at 150%: 640x360 CSS px is 960x540 physical px.
        let panel = display("display-1", 0, 0, 2560, 1600, 1.5);
        let region = region_from_css_selection(&panel, 160.0, 80.0, 640.0, 360.0)
            .expect("a CSS selection inside the panel");
        assert_eq!((region.x, region.y), (240, 120));
        assert_eq!((region.width, region.height), (960, 540));

        // The half-open edges are exactly the display's: the full selection is accepted.
        let full = region_from_css_selection(&panel, 0.0, 0.0, 2560.0 / 1.5, 1600.0 / 1.5)
            .expect("the whole panel");
        assert_eq!(
            (full.x, full.y, full.width, full.height),
            (0, 0, 2560, 1600)
        );
    }

    #[test]
    fn a_selection_that_leaves_the_display_is_refused_never_clamped() {
        let panel = display("display-1", 0, 0, 2560, 1600, 1.5);
        // Two physical pixels past the right edge: rejected, not pulled back inside.
        let error = region_from_css_selection(&panel, 0.0, 0.0, 2562.0 / 1.5, 1600.0 / 1.5)
            .expect_err("a selection past the right edge is refused");
        assert!(
            error.message.contains("outside the display"),
            "{}",
            error.message
        );
        // The half-open edge itself is inside: x + width == 2560 is valid, one more is not.
        assert!(validate_region(&canonical(&panel, 0, 0, 2560, 1600), &panel).is_ok());
        let error = validate_region(&canonical(&panel, 2, 0, 2560, 1600), &panel)
            .expect_err("one pixel past the right edge is refused");
        assert!(
            error.message.contains("does not clamp"),
            "{}",
            error.message
        );
        // A rectangle that only partly overlaps the display is refused as well.
        let overlapping = display("display-1", 1920, 0, 1920, 1080, 1.0);
        assert!(region_from_desktop_rect(&overlapping, (1800, 0, 2200, 400)).is_err());
        // A desktop rectangle outside the display's own column is refused, not folded in.
        let error = region_from_desktop_rect(&overlapping, (0, 0, 400, 400))
            .expect_err("a rectangle left of the display is refused");
        assert!(
            error.message.contains("outside the display"),
            "{}",
            error.message
        );
    }

    #[test]
    fn an_empty_foreign_or_odd_region_names_its_own_refusal() {
        let panel = display("display-1", 0, 0, 2560, 1600, 1.5);
        let empty = canonical(&panel, 0, 0, 0, 200);
        let error = validate_region(&empty, &panel).expect_err("an empty region is refused");
        assert!(
            error.message.contains("empty selection"),
            "{}",
            error.message
        );

        let mut foreign = canonical(&panel, 0, 0, 400, 200);
        foreign.display_id = DisplayId("display-9".into());
        let error = validate_region(&foreign, &panel).expect_err("a foreign region is refused");
        assert!(error.message.contains("display-9"), "{}", error.message);

        // 4:2:0 needs even dimensions; an odd region is refused instead of padded.
        let odd = canonical(&panel, 0, 0, 401, 200);
        let error = validate_region(&odd, &panel).expect_err("an odd region is refused");
        assert!(error.message.contains("4:2:0"), "{}", error.message);

        // A display without physical pixels has no region space at all.
        let empty_display = display("display-3", 0, 0, 0, 0, 1.0);
        assert!(display_pixel_bounds(&empty_display).is_err());
    }
}
pub mod audio_admission;
