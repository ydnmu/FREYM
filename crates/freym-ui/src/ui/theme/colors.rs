use gpui::{Hsla, rgb, rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Background,
    PrimarySurface,
    DockSurface,
    ElevatedSurface,
    PreviewSurface,
    RowSurface,
    HoverSurface,
    PressedSurface,
    PrimaryText,
    SecondaryText,
    MutedText,
    DisabledText,
    SubtleBorder,
    DefaultBorder,
    ActiveBorder,
    Accent,
    AccentHover,
    AccentPressed,
    Record,
    RecordHover,
    RecordPressed,
    OnAccent,
    Danger,
    Warning,
    Error,
    Scrim,
    CaptureScrim,
    CaptureSelection,
    SelectionText,
}
impl Color {
    pub fn get(self) -> Hsla {
        super::surface_treatment::color(self, self.base())
    }
    pub(crate) fn base(self) -> Hsla {
        let white = super::ThemeId::current() == super::ThemeId::White;
        let pair = |dark, light| rgb(if white { light } else { dark }).into();
        let accent: Hsla =
            rgb(super::registry::accent().unwrap_or(if white { 0x6842c2 } else { 0xb29aef }))
                .into();
        match self {
            Self::Background => pair(0x000000, 0xf4f5f8),
            Self::PrimarySurface => pair(0x101010, 0xffffff),
            Self::DockSurface => pair(0x000000, 0xffffff),
            Self::ElevatedSurface => pair(0x191919, 0xffffff),
            Self::PreviewSurface => pair(0x121214, 0xeff0f5),
            Self::RowSurface => pair(0x191919, 0xf4f5f8),
            Self::HoverSurface => pair(0x242424, 0xe9ebf2),
            Self::PressedSurface => pair(0x262626, 0xdfe2ec),
            Self::PrimaryText => pair(0xf5f5f5, 0x1c1e26),
            Self::SecondaryText => pair(0xb5b5b5, 0x4e5362),
            Self::MutedText => pair(0xb5b5b5, 0x5b6070),
            Self::DisabledText => pair(0x777777, 0x858b99),
            Self::SubtleBorder => pair(0x2c2c2c, 0xdde0e9),
            Self::DefaultBorder => pair(0x2c2c2c, 0xc6cbd8),
            Self::Accent | Self::ActiveBorder => accent,
            Self::AccentHover => Hsla {
                l: (accent.l + if white { -0.06 } else { 0.06 }).clamp(0., 1.),
                ..accent
            },
            Self::AccentPressed => Hsla {
                l: (accent.l - 0.10).max(0.),
                ..accent
            },
            Self::OnAccent => {
                // Choose readable ink even for a custom accent.
                let c: gpui::Rgba = accent.into();
                let linear = |v: f32| {
                    if v <= 0.04045 {
                        v / 12.92
                    } else {
                        ((v + 0.055) / 1.055).powf(2.4)
                    }
                };
                let l = 0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b);
                rgb(if l > 0.179 { 0x101018 } else { 0xffffff }).into()
            }
            Self::Record => pair(0xeb6977, 0xbb3040),
            Self::RecordHover => pair(0xf57b88, 0xa22635),
            Self::RecordPressed => pair(0xe85f70, 0x8f2030),
            Self::Danger | Self::Error => pair(0xf19389, 0xa52c3c),
            Self::Warning => pair(0xe8ba6d, 0x875b12),
            Self::Scrim => rgba(if white { 0x11152250 } else { 0x000000b3 }).into(),
            Self::CaptureScrim => rgba(0x00000066).into(),
            Self::CaptureSelection => rgba(0xffffff12).into(),
            Self::SelectionText => accent.opacity(0.20),
        }
    }
}
