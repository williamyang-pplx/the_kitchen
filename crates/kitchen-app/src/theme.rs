//! Fonts and colors.

use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x26);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x16, 0x1e);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x13, 0x13, 0x1a);
pub const BORDER: Color32 = Color32::from_rgb(0x29, 0x2e, 0x42);
pub const TEXT: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
pub const DIM: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);
/// Secondary text that still has to be read: ~8:1 against `PANEL`, where
/// `DIM` is under 3:1.
pub const MUTED: Color32 = Color32::from_rgb(0xa9, 0xb1, 0xd6);
/// Text of the selected tab, a step brighter than `TEXT`.
pub const BRIGHT: Color32 = Color32::from_rgb(0xe6, 0xea, 0xff);
pub const ACCENT: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
pub const GREEN: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a);
pub const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
pub const YELLOW: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
pub const PURPLE: Color32 = Color32::from_rgb(0xbb, 0x9a, 0xf7);
pub const CYAN: Color32 = Color32::from_rgb(0x7d, 0xcf, 0xff);
/// Bright violet marking the focused pane.
pub const FOCUS: Color32 = Color32::from_rgb(0xb2, 0x6b, 0xff);
pub const SELECTED: Color32 = Color32::from_rgb(0x28, 0x2d, 0x44);
pub const ADDED_BG: Color32 = Color32::from_rgb(0x1f, 0x33, 0x2a);
pub const REMOVED_BG: Color32 = Color32::from_rgb(0x3b, 0x22, 0x2a);

/// System fonts used as fallbacks so box drawing, symbols and CJK render.
const FALLBACKS: &[(&str, &str)] = &[
    ("apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf"),
    ("arial-unicode", "/System/Library/Fonts/Supplemental/Arial Unicode.ttf"),
    ("hiragino-gb", "/System/Library/Fonts/Hiragino Sans GB.ttc"),
];

/// macOS system face (SF Pro) for tab and workspace labels, as in cmux.
const UI_FONT: &str = "/System/Library/Fonts/SFNS.ttf";

/// Font for tab and workspace labels: SF Pro, falling back to the
/// default proportional font when it isn't installed.
pub fn ui_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("ui".into()))
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    if let Ok(bytes) = std::fs::read("/System/Library/Fonts/Menlo.ttc") {
        fonts.font_data.insert("menlo".into(), FontData::from_owned(bytes).into());
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "menlo".into());
    }
    for (name, path) in FALLBACKS {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert((*name).into(), FontData::from_owned(bytes).into());
            for fam in [FontFamily::Monospace, FontFamily::Proportional] {
                fonts.families.entry(fam).or_default().push((*name).into());
            }
        }
    }
    let mut ui = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Ok(bytes) = std::fs::read(UI_FONT) {
        fonts.font_data.insert("sf-pro".into(), FontData::from_owned(bytes).into());
        ui.insert(0, "sf-pro".into());
    }
    fonts.families.insert(FontFamily::Name("ui".into()), ui);
    ctx.set_fonts(fonts);

    let mut v = Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = SELECTED;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = Color32::from_rgb(0x33, 0x46, 0x7c);
    v.widgets.noninteractive.bg_stroke.color = BORDER;
    v.hyperlink_color = ACCENT;
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.interaction.selectable_labels = false;
    });
}
