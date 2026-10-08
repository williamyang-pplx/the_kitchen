//! Fonts and colors.

use egui::epaint::text::VariationCoords;
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Shadow, Stroke, Theme, ThemePreference, Visuals};

// Surfaces, darkest to lightest (Tokyo Night). Chrome (sidebar, title bar,
// tab strip, status bar) sits on `SIDEBAR`; content on `BG`; dialogs and
// popups float on `OVERLAY`.
pub const BG: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x26);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x16, 0x1e);
pub const SIDEBAR: Color32 = PANEL;
pub const OVERLAY: Color32 = Color32::from_rgb(0x24, 0x28, 0x3b);
/// Hover fill for rows and flat buttons on the chrome.
pub const HOVER: Color32 = Color32::from_rgb(0x1f, 0x23, 0x35);
pub const SELECTED: Color32 = Color32::from_rgb(0x29, 0x2e, 0x42);
pub const BORDER: Color32 = Color32::from_rgb(0x29, 0x2e, 0x42);

// Text, brightest to faintest.
pub const TEXT: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
/// Secondary text that still has to be read: ~8:1 against `PANEL`.
pub const MUTED: Color32 = Color32::from_rgb(0xa9, 0xb1, 0xd6);
/// Tertiary text (subtitles, hints): ~4.5:1 against `PANEL`.
pub const SUBTLE: Color32 = Color32::from_rgb(0x73, 0x7a, 0xa2);
/// Decoration more than text (line numbers, placeholders): under 3:1.
pub const DIM: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);
pub const FAINT: Color32 = Color32::from_rgb(0x3b, 0x42, 0x61);

/// Selected or focused, and nothing else.
pub const ACCENT: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
/// Added / success.
pub const GREEN: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a);
/// Removed / error.
pub const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
/// Needs you: unread, in progress.
pub const YELLOW: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
pub const PURPLE: Color32 = Color32::from_rgb(0xbb, 0x9a, 0xf7);
pub const CYAN: Color32 = Color32::from_rgb(0x7d, 0xcf, 0xff);
/// Bright violet marking the focused pane.
pub const FOCUS: Color32 = Color32::from_rgb(0xb2, 0x6b, 0xff);
pub const ADDED_BG: Color32 = Color32::from_rgb(0x1f, 0x33, 0x2a);
pub const REMOVED_BG: Color32 = Color32::from_rgb(0x3b, 0x22, 0x2a);
pub const SELECTION: Color32 = Color32::from_rgb(0x33, 0x46, 0x7c);

/// Type scale for UI text (code uses the `font_size` setting).
pub const TEXT_SMALL: f32 = 11.0;
pub const TEXT_BODY: f32 = 12.5;
pub const TEXT_TITLE: f32 = 14.0;
pub const TEXT_HEADING: f32 = 18.0;
/// Uppercase section labels ("WORKSPACES").
pub const TEXT_LABEL: f32 = 10.5;

/// Spacing steps.
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 16.0;
pub const SPACE_XL: f32 = 24.0;

pub const RADIUS_S: u8 = 4;
pub const RADIUS: u8 = 6;
pub const RADIUS_L: u8 = 10;

/// System fonts used as fallbacks so box drawing, symbols and CJK render.
const FALLBACKS: &[(&str, &str)] = &[
    ("apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf"),
    ("arial-unicode", "/System/Library/Fonts/Supplemental/Arial Unicode.ttf"),
    ("hiragino-gb", "/System/Library/Fonts/Hiragino Sans GB.ttc"),
];

/// macOS system face (SF Pro) for tab and workspace labels, as in cmux.
const UI_FONT: &str = "/System/Library/Fonts/SFNS.ttf";

/// SFNS.ttf is variable and defaults to the Display cut (`opsz` 28), whose
/// tight spacing is meant for headlines. CoreText switches to the Text cut
/// below 20pt; pin it here since every label is smaller than that.
const UI_OPSZ: f32 = 17.0;

/// Font for tab and workspace labels: SF Pro, falling back to the
/// default proportional font when it isn't installed.
pub fn ui_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("ui".into()))
}

/// Semibold `ui_font`, for status lines, badges and section headers.
pub fn ui_font_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("ui-semibold".into()))
}

/// Bold `ui_font`, for workspace names and dialog headings.
pub fn ui_font_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("ui-bold".into()))
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
    let proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let bytes = std::fs::read(UI_FONT).ok();
    for (family, weight) in [("ui", 400.0), ("ui-semibold", 600.0), ("ui-bold", 700.0)] {
        let mut list = proportional.clone();
        if let Some(bytes) = &bytes {
            let tweak = FontTweak { coords: VariationCoords::new([(b"opsz", UI_OPSZ), (b"wght", weight)]), ..Default::default() };
            fonts.font_data.insert(family.into(), FontData::from_owned(bytes.clone()).tweak(tweak).into());
            list.insert(0, family.into());
        }
        fonts.families.insert(FontFamily::Name(family.into()), list);
    }
    // Proportional text is SF Pro too, so labels sized with `RichText::size`
    // match the ones drawn with `ui_font`.
    if bytes.is_some() {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
    }
    ctx.set_fonts(fonts);

    // The app is dark whatever macOS is set to: following a light system
    // theme gave stock widgets (dialogs, fields, menus) light colors.
    ctx.set_theme(ThemePreference::Dark);
    ctx.set_visuals_of(Theme::Dark, visuals());
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.spacing.interact_size.y = 24.0;
        s.spacing.window_margin = egui::Margin::same(SPACE_L as i8);
        s.spacing.menu_margin = egui::Margin::same(6);
        s.interaction.selectable_labels = false;
        s.text_styles.insert(egui::TextStyle::Heading, ui_font_bold(TEXT_HEADING));
        s.text_styles.insert(egui::TextStyle::Body, ui_font(TEXT_BODY));
        s.text_styles.insert(egui::TextStyle::Button, ui_font(TEXT_BODY));
        s.text_styles.insert(egui::TextStyle::Small, ui_font(TEXT_SMALL));
    });
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = OVERLAY;
    v.extreme_bg_color = PANEL;
    v.faint_bg_color = HOVER;
    v.code_bg_color = PANEL;
    v.override_text_color = Some(TEXT);
    v.weak_text_color = Some(DIM);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = YELLOW;
    v.error_fg_color = RED;
    v.selection.bg_fill = SELECTION;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.text_cursor.stroke = Stroke::new(2.0, ACCENT);
    v.window_corner_radius = CornerRadius::same(RADIUS_L);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_shadow = Shadow { offset: [0, 10], blur: 32, spread: 0, color: Color32::from_black_alpha(120) };
    v.window_highlight_topmost = false;
    v.menu_corner_radius = CornerRadius::same(8);
    v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(100) };
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = OVERLAY;
    w.noninteractive.weak_bg_fill = OVERLAY;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    let fill = |w: &mut egui::style::WidgetVisuals, bg: Color32, stroke: Color32, fg: Color32| {
        w.bg_fill = bg;
        w.weak_bg_fill = bg;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.fg_stroke = Stroke::new(1.0, fg);
        w.corner_radius = CornerRadius::same(RADIUS);
        w.expansion = 0.0;
    };
    fill(&mut w.inactive, SELECTED, SELECTED, MUTED);
    fill(&mut w.hovered, FAINT, FAINT, TEXT);
    fill(&mut w.active, Color32::from_rgb(0x41, 0x48, 0x68), ACCENT, TEXT);
    fill(&mut w.open, SELECTED, BORDER, TEXT);
    v
}
