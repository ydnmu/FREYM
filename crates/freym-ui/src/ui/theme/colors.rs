use gpui::{Hsla, rgb, rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Background,
    PrimarySurface,
    ElevatedSurface,
    PreviewSurface,
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
        let theme = super::ThemeId::current();
        if theme != super::ThemeId::ClassicLight {
            return self.for_theme(theme);
        }
        match self {
            Self::Background => rgb(0xf2f2eb).into(),
            Self::PrimarySurface => rgba(0xffffffd6).into(),
            Self::ElevatedSurface => rgba(0xfffffff0).into(),
            Self::PreviewSurface => rgba(0xffffffb8).into(),
            Self::HoverSurface => rgba(0xfffffff0).into(),
            Self::PressedSurface => Self::Accent.get().opacity(0.12),
            Self::PrimaryText => rgb(0x313635).into(),
            Self::SecondaryText => rgb(0x666e64).into(),
            Self::MutedText => rgb(0x747c72).into(),
            Self::DisabledText => rgb(0x959a93).into(),
            Self::SubtleBorder => Self::Accent.get().opacity(0.12),
            Self::DefaultBorder => rgba(0xffffffb0).into(),
            Self::ActiveBorder | Self::Accent => rgb(0x65776c).into(),
            Self::AccentHover => rgb(0x596a60).into(),
            Self::AccentPressed => rgb(0x4d5e53).into(),
            Self::Record => rgb(0xbb3040).into(),
            Self::RecordHover => rgb(0xa22635).into(),
            Self::RecordPressed => rgb(0x8f2030).into(),
            Self::OnAccent => rgb(0xffffff).into(),
            Self::Danger => rgb(0xc8665e).into(),
            Self::Warning => rgb(0xb68a42).into(),
            Self::Error => rgb(0xa74e46).into(),
            Self::Scrim => rgba(0x3a40371c).into(),
            Self::CaptureScrim => rgba(0x141b1940).into(),
            Self::CaptureSelection => rgba(0xffffff12).into(),
            Self::SelectionText => Self::Accent.get().opacity(0.18),
        }
    }
    /// Classic Light above is the exact pre-redesign native palette.
    fn for_theme(self, theme: super::ThemeId) -> Hsla {
        if matches!(
            theme,
            super::ThemeId::FreymExperimental | super::ThemeId::RaycastGlass
        ) {
            let experimental = theme == super::ThemeId::FreymExperimental;
            return match self {
                Self::Background => rgb(if experimental { 0x5912ff } else { 0x09090b }).into(),
                Self::PrimarySurface | Self::PreviewSurface => {
                    rgb(if experimental { 0x15121b } else { 0x18181b }).into()
                }
                Self::ElevatedSurface => rgb(if experimental { 0x24202b } else { 0x232326 }).into(),
                Self::HoverSurface => rgb(if experimental { 0x322052 } else { 0x303034 }).into(),
                Self::PressedSurface => rgb(if experimental { 0x27183f } else { 0x38383d }).into(),
                Self::PrimaryText => rgb(if experimental { 0xf2edff } else { 0xf5f5f6 }).into(),
                Self::SecondaryText => rgb(if experimental { 0xc7bfd5 } else { 0xc0bec2 }).into(),
                Self::MutedText => rgb(if experimental { 0xb4a9c5 } else { 0xaaa7ae }).into(),
                Self::DisabledText => rgb(0x77737f).into(),
                Self::SubtleBorder => rgb(if experimental { 0x30283b } else { 0x303034 }).into(),
                Self::DefaultBorder => rgb(if experimental { 0x3b324b } else { 0x414145 }).into(),
                Self::Accent | Self::ActiveBorder => {
                    rgb(if experimental { 0xaa8bff } else { 0xff6363 }).into()
                }
                Self::AccentHover => rgb(if experimental { 0xbba2ff } else { 0xff8585 }).into(),
                Self::AccentPressed => rgb(if experimental { 0x9570ed } else { 0xe95353 }).into(),
                Self::Record => rgb(if experimental { 0xeb6977 } else { 0xff6363 }).into(),
                Self::RecordHover => rgb(0xff8585).into(),
                Self::RecordPressed => rgb(0xd95363).into(),
                Self::OnAccent => rgb(0x1c1012).into(),
                Self::Danger | Self::Error => rgb(0xff9999).into(),
                Self::Warning => rgb(0xe8ba6d).into(),
                Self::Scrim => rgba(0x000000b3).into(),
                Self::CaptureScrim => rgba(0x00000066).into(),
                Self::CaptureSelection => rgba(0xffffff12).into(),
                Self::SelectionText => rgba(0xaa8bff40).into(),
            };
        }
        if theme == super::ThemeId::AmoledBlack {
            return match self {
                Self::Background => rgb(0x000000).into(),
                Self::PrimarySurface | Self::PreviewSurface => rgb(0x101010).into(),
                Self::ElevatedSurface => rgb(0x191919).into(),
                Self::HoverSurface => rgb(0x242424).into(),
                Self::PressedSurface => rgb(0x262626).into(),
                Self::PrimaryText => rgb(0xf5f5f5).into(),
                Self::SecondaryText | Self::MutedText => rgb(0xb5b5b5).into(),
                Self::DisabledText => rgb(0x777777).into(),
                Self::SubtleBorder | Self::DefaultBorder => rgb(0x2c2c2c).into(),
                Self::ActiveBorder => rgb(0xd0d0d0).into(),
                Self::Accent => rgb(0xd0d0d0).into(),
                Self::AccentHover => rgb(0xf5f5f5).into(),
                Self::AccentPressed => rgb(0xb5b5b5).into(),
                Self::OnAccent => rgb(0x111111).into(),
                Self::Record => rgb(0xeb6977).into(),
                Self::RecordHover => rgb(0xf57b88).into(),
                Self::RecordPressed => rgb(0xe85f70).into(),
                Self::Danger | Self::Error => rgb(0xf19389).into(),
                Self::Warning => rgb(0xe8ba6d).into(),
                Self::Scrim => rgba(0x000000b3).into(),
                Self::CaptureScrim => rgba(0x00000066).into(),
                Self::CaptureSelection => rgba(0xffffff12).into(),
                Self::SelectionText => rgba(0xffffff28).into(),
            };
        }
        let motion = theme.is_motion();
        let solid = |classic, modern| rgb(if motion { modern } else { classic }).into();
        match self {
            Self::Background => solid(0x181818, 0x0b0c0f),
            Self::PrimarySurface => solid(0x282828, 0x18191d),
            Self::ElevatedSurface => solid(0x323232, 0x27282e),
            Self::PreviewSurface => solid(0x282828, 0x101114),
            Self::HoverSurface => solid(0x3a3a3a, 0x303138),
            Self::PressedSurface => solid(0x3d3d3d, 0x393a42),
            Self::PrimaryText => solid(0xf4f4f4, 0xf5f5f7),
            Self::SecondaryText => solid(0xb8b8b8, 0xb9bac5),
            Self::MutedText => solid(0xb8b8b8, 0x999ba9),
            Self::DisabledText => rgb(0x71737f).into(),
            Self::SubtleBorder => solid(0x414141, 0x25262c),
            Self::DefaultBorder => solid(0x414141, 0x3c3d47),
            Self::Accent | Self::ActiveBorder if motion => super::material::accent(),
            Self::AccentHover if motion => rgb(if theme == super::ThemeId::EmberMotion {
                0xd42c43
            } else {
                0x8250e9
            })
            .into(),
            Self::AccentPressed if motion => rgb(if theme == super::ThemeId::EmberMotion {
                0xac2035
            } else {
                0x6031c5
            })
            .into(),
            Self::Accent | Self::ActiveBorder | Self::AccentHover | Self::AccentPressed => {
                rgb(0xd4d4d4).into()
            }
            Self::Record => rgb(0xbb3040).into(),
            Self::RecordHover => rgb(0xa22635).into(),
            Self::RecordPressed => rgb(0x8f2030).into(),
            Self::OnAccent => rgb(if motion { 0xffffff } else { 0x111111 }).into(),
            Self::Danger | Self::Error => rgb(0xff747b).into(),
            Self::Warning => rgb(0xe8ba6d).into(),
            Self::Scrim => rgba(0x000000a8).into(),
            Self::CaptureScrim => rgba(0x00000066).into(),
            Self::CaptureSelection => rgba(0xffffff12).into(),
            Self::SelectionText => rgba(0x7441df55).into(),
        }
    }
}
