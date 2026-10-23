//! Shared look and small controls for the edge-kvm windows.

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Frame, Margin, Response, RichText, Sense, Stroke,
    TextStyle, Ui, Vec2,
};

pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x8c, 0xff);
pub const OK: Color32 = Color32::from_rgb(0x3e, 0xcf, 0x8e);
pub const BAD: Color32 = Color32::from_rgb(0xf0, 0x71, 0x78);
pub const MUTED: Color32 = Color32::from_rgb(0x9a, 0xa1, 0xad);
pub const FAINT: Color32 = Color32::from_rgb(0x6b, 0x72, 0x80);
pub const WINDOW: Color32 = Color32::from_rgb(0x1c, 0x1f, 0x25);
pub const BAR: Color32 = Color32::from_rgb(0x19, 0x1c, 0x21);
pub const FIELD: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x36);
pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x37);

/// Dark, compact, one accent colour.
pub fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = WINDOW;
    style.visuals.window_fill = WINDOW;
    style.visuals.extreme_bg_color = FIELD;
    style.visuals.selection.bg_fill = ACCENT;
    style.visuals.hyperlink_color = ACCENT;
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(6);
    }
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x2c, 0x31, 0x3a);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x34, 0x3a, 0x45);
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 4.0);
    style.spacing.interact_size.y = 26.0;
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(13.5));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.5));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(11.5));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(12.5));
    ctx.set_global_style(style);
}

/// A small uppercase heading over a group of rows.
pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title.to_uppercase()).small().color(FAINT));
    ui.add_space(2.0);
}

/// A row label in the left column.
pub fn row_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(MUTED));
}

/// An on/off switch. Returns the response, marked changed when flipped.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(34.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let off = Color32::from_rgb(0x3b, 0x40, 0x4a);
        let fill = off.lerp_to_gamma(ACCENT, t);
        let enabled = ui.is_enabled();
        let fill = if enabled {
            fill
        } else {
            fill.gamma_multiply(0.5)
        };
        ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
        let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
        ui.painter().circle_filled(
            egui::pos2(x, rect.center().y),
            8.0,
            Color32::from_gray(if *on { 255 } else { 207 }),
        );
    }
    response
}

/// Mutually exclusive options in one strip. Returns whether the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    Frame::new()
        .fill(FIELD)
        .stroke(Stroke::new(1.0, Color32::from_rgb(0x3a, 0x3f, 0x49)))
        .corner_radius(7)
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().button_padding = Vec2::new(9.0, 2.0);
            ui.horizontal(|ui| {
                for (option, label) in options {
                    if ui.selectable_label(*value == *option, *label).clicked() && *value != *option
                    {
                        *value = *option;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// A filled dot showing state.
pub fn status_dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// The main action button.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(ACCENT))
}
