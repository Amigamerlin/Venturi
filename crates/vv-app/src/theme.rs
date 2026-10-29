//! Colors that give the interface its identity, taken from the logo
//! (`media/icons/svg/vv-icon.svg`).

use egui::Color32;
use std::borrow::Cow;

/// Settings > General. Dark by default: the timeline and the viewer are
/// drawn for it, and a light system theme would otherwise win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    System,
    #[default]
    Dark,
    Light,
}

impl ThemeChoice {
    pub const ALL: [Self; 3] = [Self::System, Self::Dark, Self::Light];

    /// Key in the settings file: must never be changed.
    pub fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.id() == id)
    }

    pub fn label(self) -> Cow<'static, str> {
        match self {
            Self::System => t!("settings.theme_system"),
            Self::Dark => t!("settings.theme_dark"),
            Self::Light => t!("settings.theme_light"),
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        ctx.set_theme(match self {
            Self::System => egui::ThemePreference::System,
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
        });
    }
}

/// The orange of the logo: selection, active toggles, keyframes.
pub(crate) const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x6A, 0x1F);
/// The accent darkened enough to carry light text on top of it.
pub(crate) const ACCENT_FILL: Color32 = Color32::from_rgb(0xA8, 0x45, 0x14);
/// Translucent accent for box selections.
pub(crate) const ACCENT_TRANSLUCENT: Color32 = Color32::from_rgba_premultiplied(40, 17, 5, 40);
/// Errors: media offline, audio clipping.
pub(crate) const ERROR: Color32 = Color32::from_rgb(230, 70, 70);
pub(crate) const PLAYHEAD: Color32 = Color32::from_rgb(220, 50, 50);

pub(crate) fn apply(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.visuals.selection.bg_fill = ACCENT_FILL;
            style.visuals.selection.stroke.color = Color32::WHITE;
            style.visuals.hyperlink_color = ACCENT;
        });
    }
}
