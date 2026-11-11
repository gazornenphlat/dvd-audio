//! The retro-serigraph look from the design guideline: four inks on warm paper, crisp outlines,
//! sharp corners on controls and rounded cards, and orange for the main action.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, Margin, Rounding, Stroke, TextStyle, Ui};

pub const IVORY: Color32 = Color32::from_rgb(0xF9, 0xF5, 0xEA);
pub const NAVY: Color32 = Color32::from_rgb(0x14, 0x22, 0x3A);
pub const ORANGE: Color32 = Color32::from_rgb(0xE6, 0x5A, 0x28);
pub const ORANGE_HOVER: Color32 = Color32::from_rgb(0xD0, 0x4D, 0x1E);
pub const ORANGE_ACTIVE: Color32 = Color32::from_rgb(0xB8, 0x3E, 0x14);
pub const BLACK: Color32 = Color32::from_rgb(0x0D, 0x0E, 0x11);

/// Applies the palette, outlines and type to the whole window. Called once at startup.
pub fn apply(ctx: &egui::Context) {
    install_font(ctx);
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = IVORY;
    visuals.window_fill = IVORY;
    visuals.extreme_bg_color = IVORY;
    visuals.override_text_color = Some(BLACK);
    visuals.hyperlink_color = ORANGE;
    visuals.selection.bg_fill = ORANGE;
    visuals.selection.stroke = Stroke::new(1.0, IVORY);

    let square = Rounding::same(0.0);
    let line = Stroke::new(1.5, NAVY);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = IVORY;
    widgets.noninteractive.weak_bg_fill = IVORY;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, BLACK);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, BLACK);
    // Default (secondary) controls: ivory with a navy outline and navy text.
    for state in [&mut widgets.inactive, &mut widgets.open] {
        state.bg_fill = IVORY;
        state.weak_bg_fill = IVORY;
        state.bg_stroke = line;
        state.fg_stroke = Stroke::new(1.0, NAVY);
        state.rounding = square;
    }
    // Hover and press: the orange shifts darker, as the guideline asks.
    for (state, fill) in [(&mut widgets.hovered, ORANGE_HOVER), (&mut widgets.active, ORANGE_ACTIVE)] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.5, BLACK);
        state.fg_stroke = Stroke::new(1.0, IVORY);
        state.rounding = square;
    }

    ctx.style_mut(|style| {
        style.visuals = visuals;
        style.text_styles.insert(TextStyle::Heading, FontId::new(30.0, FontFamily::Proportional));
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    });
}

/// The main action: orange with ivory text and a black outline.
pub fn primary(ui: &mut Ui, enabled: bool, text: &str) -> egui::Response {
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        for (state, fill) in [
            (&mut widgets.inactive, ORANGE),
            (&mut widgets.hovered, ORANGE_HOVER),
            (&mut widgets.active, ORANGE_ACTIVE),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::new(1.5, BLACK);
            state.fg_stroke = Stroke::new(1.0, IVORY);
        }
        ui.add_enabled(enabled, egui::Button::new(text))
    })
    .inner
}

/// A panel with a crisp outline and softly rounded corners, for the track list and the results.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::none()
        .fill(IVORY)
        .stroke(Stroke::new(1.0, BLACK))
        .rounding(Rounding::same(16.0))
        .inner_margin(Margin::same(10.0))
        .show(ui, add)
        .inner
}

/// Loads Segoe UI from Windows, if it is there; otherwise the built-in font stays.
fn install_font(ctx: &egui::Context) {
    let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") else {
        return;
    };
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("segoe".to_owned(), FontData::from_owned(bytes).into());
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".to_owned());
    ctx.set_fonts(fonts);
}
