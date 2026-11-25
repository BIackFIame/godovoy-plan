use std::sync::Arc;

use eframe::egui::{
    self, Align, Button, Color32, FontData, FontDefinitions, FontFamily, FontId, Frame, Layout,
    Margin, RichText, Stroke, TextStyle, Ui, vec2,
};

// The paper surface holds content; the warm neutral separates secondary panels.
pub const PAPER: Color32 = Color32::from_rgb(255, 255, 255);
pub const BACK: Color32 = Color32::from_rgb(246, 246, 242);
pub const NAV: Color32 = PAPER;
pub const INK: Color32 = Color32::from_rgb(35, 41, 36);
pub const MUTED: Color32 = Color32::from_rgb(86, 96, 91);
pub const BORDER: Color32 = Color32::from_rgb(220, 223, 216);
pub const CONTROL_BORDER: Color32 = Color32::from_rgb(132, 139, 135);
pub const ACCENT: Color32 = Color32::from_rgb(36, 85, 170);
pub const ACTION_TEXT: Color32 = ACCENT;
pub const TINT: Color32 = Color32::from_rgb(232, 239, 251);
pub const RED: Color32 = Color32::from_rgb(159, 50, 39);
pub const GREEN: Color32 = Color32::from_rgb(39, 96, 70);

pub const DUE_TODAY: Color32 = Color32::from_rgb(134, 80, 0);
pub const MODAL_BACKDROP: Color32 = Color32::from_black_alpha(62);
pub const CONTROL_HEIGHT: f32 = 36.0;
pub const COMPACT_HEIGHT: f32 = 32.0;
pub const PAGE_MARGIN: Margin = Margin::symmetric(24, 18);
pub const PAGE_TITLE_SIZE: f32 = 24.0;
pub const DIALOG_TITLE_SIZE: f32 = 20.0;
pub const SECTION_TITLE_SIZE: f32 = 16.0;

pub fn page_frame() -> Frame {
    Frame::new().fill(PAPER).inner_margin(PAGE_MARGIN)
}

/// A single line of body text and its vertical padding make up one toolbar control.
pub fn field_margin(ui: &Ui) -> Margin {
    let padding = (CONTROL_HEIGHT - ui.text_style_height(&TextStyle::Body))
        .round()
        .max(0.0) as i8;
    Margin {
        left: 9,
        right: 9,
        top: padding / 2,
        bottom: padding - padding / 2,
    }
}

fn heading_family() -> FontFamily {
    FontFamily::Name("golos-strong".into())
}

pub fn heading_font(size: f32) -> FontId {
    FontId::new(size, heading_family())
}

pub fn setup(ctx: &egui::Context) {
    crate::app_icon::install(ctx);
    let mut fonts = FontDefinitions::default();
    for (name, data) in [
        (
            "golos-regular",
            include_bytes!("../assets/GolosText-Regular.ttf").as_slice(),
        ),
        (
            "golos-strong",
            include_bytes!("../assets/GolosText-Semibold.ttf").as_slice(),
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(data)));
    }
    let proportional = fonts
        .families
        .get_mut(&FontFamily::Proportional)
        .expect("egui default family");
    proportional.insert(0, "golos-regular".into());
    let mut display = proportional.clone();
    display.insert(0, "golos-strong".into());
    fonts.families.insert(heading_family(), display);
    ctx.set_fonts(fonts);

    let mut style = egui::Style {
        visuals: egui::Visuals::light(),
        ..Default::default()
    };
    let visuals = &mut style.visuals;
    visuals.panel_fill = BACK;
    visuals.window_fill = PAPER;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_highlight_topmost = false;
    visuals.extreme_bg_color = PAPER;
    visuals.faint_bg_color = BACK;
    visuals.weak_text_color = Some(MUTED);
    visuals.hyperlink_color = ACTION_TEXT;
    visuals.warn_fg_color = DUE_TODAY;
    visuals.error_fg_color = RED;
    visuals.text_cursor.stroke.color = ACCENT;
    visuals.selection.bg_fill = TINT;
    visuals.selection.stroke = Stroke::new(1.5, ACCENT);
    for (widget, fill, stroke, text) in [
        (&mut visuals.widgets.noninteractive, PAPER, BORDER, INK),
        (&mut visuals.widgets.inactive, PAPER, CONTROL_BORDER, INK),
        (&mut visuals.widgets.hovered, TINT, ACCENT, INK),
        (&mut visuals.widgets.active, TINT, ACCENT, INK),
        (&mut visuals.widgets.open, TINT, ACCENT, INK),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(1.0, stroke);
        widget.fg_stroke = Stroke::new(widget.fg_stroke.width, text);
        widget.corner_radius = 5.into();
        widget.expansion = 0.0;
    }
    visuals.widgets.active.bg_stroke = Stroke::new(2.0, ACCENT);
    style.spacing.item_spacing = vec2(8.0, 7.0);
    style.spacing.interact_size = vec2(40.0, CONTROL_HEIGHT);
    style.spacing.button_padding = vec2(12.0, 7.0);
    style.spacing.scroll = egui::style::ScrollStyle {
        floating_width: 3.0,
        foreground_color: true,
        dormant_background_opacity: 0.0,
        dormant_handle_opacity: 0.30,
        active_handle_opacity: 0.60,
        interact_handle_opacity: 0.85,
        ..egui::style::ScrollStyle::thin()
    };
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(12.0));
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Heading, heading_font(DIALOG_TITLE_SIZE));
    ctx.set_theme(egui::Theme::Light);
    ctx.set_style_of(egui::Theme::Light, style);
}

pub fn semibold(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text.into())
        .font(heading_font(size))
        .color(color)
}

pub fn title(ui: &mut Ui, text: impl Into<String>, size: f32) {
    ui.label(semibold(text, size, INK));
}

pub fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(MUTED).size(13.0));
}

pub fn card() -> Frame {
    Frame::new()
        .fill(PAPER)
        .inner_margin(Margin::same(18))
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(8)
}

pub fn dialog(ctx: &egui::Context) -> Frame {
    Frame::window(&ctx.global_style())
        .fill(PAPER)
        .inner_margin(Margin::same(22))
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(10)
}

pub fn primary(text: &str) -> Button<'_> {
    Button::new(semibold(text, 14.0, PAPER))
        .fill(ACCENT)
        .corner_radius(5)
        .min_size(vec2(0.0, CONTROL_HEIGHT))
}

pub fn quiet(text: &str) -> Button<'_> {
    Button::new(RichText::new(text).size(13.0).color(ACTION_TEXT))
        .frame_when_inactive(false)
        .corner_radius(5)
        .min_size(vec2(0.0, CONTROL_HEIGHT))
}

pub fn marker(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 9.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

pub fn brand(ui: &mut Ui) {
    ui.horizontal(|ui| {
        brand_mark(ui);
        ui.label(semibold("Годовой план", 15.0, INK));
    });
}

pub fn workspace_brand(ui: &mut Ui, workspace: &str) {
    ui.horizontal(|ui| {
        brand_mark(ui);
        ui.allocate_ui_with_layout(vec2(132.0, 32.0), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(semibold("Годовой план", 14.0, INK));
            ui.add(egui::Label::new(RichText::new(workspace).size(11.5).color(MUTED)).truncate())
                .on_hover_text(workspace);
        });
    });
}

fn brand_mark(ui: &mut Ui) {
    crate::app_icon::show(ui);
}

pub fn section_heading(ui: &mut Ui, text: &str, annotation: &str) {
    ui.horizontal(|ui| {
        title(ui, text, SECTION_TITLE_SIZE);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            muted(ui, annotation)
        });
    });
}

pub fn nav(ui: &mut Ui, name: &str, active: bool) -> bool {
    let item = choice_button(ui, name, active, None);
    if active {
        let left = item.rect.left_bottom() + vec2(10.0, -1.0);
        let right = item.rect.right_bottom() - vec2(10.0, 1.0);
        ui.painter()
            .line_segment([left, right], Stroke::new(2.0, ACCENT));
    }
    item.clicked()
}

pub fn view_option(ui: &mut Ui, name: &str, active: bool) -> bool {
    choice_button(ui, name, active, Some(BACK)).clicked()
}

fn choice_button(
    ui: &mut Ui,
    name: &str,
    active: bool,
    selection_fill: Option<Color32>,
) -> egui::Response {
    let regular = FontId::proportional(13.0);
    let bold = heading_font(13.0);
    // Reserve both text widths so selecting a tab cannot move its neighbours.
    let label_width = ui.fonts_mut(|fonts| {
        let mut width = |font| fonts.layout_no_wrap(name.to_owned(), font, INK).size().x;
        width(regular.clone()).max(width(bold.clone()))
    });
    let min_width = (label_width + 2.0 * ui.spacing().button_padding.x).max(COMPACT_HEIGHT);
    let text = RichText::new(name)
        .font(if active { bold } else { regular })
        .color(if active { INK } else { MUTED });
    let mut button = Button::new(text)
        .frame_when_inactive(active && selection_fill.is_some())
        .stroke(Stroke::NONE)
        .corner_radius(5)
        .min_size(vec2(min_width, COMPACT_HEIGHT));
    if active && let Some(fill) = selection_fill {
        button = button.fill(fill);
    }
    let response = ui.add(button);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            active,
            name,
        )
    });
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.shrink(1.0),
            5,
            Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_a_tab_preserves_the_position_of_the_next_tab() {
        let ctx = egui::Context::default();
        setup(&ctx);
        let _ = ctx.run_ui(Default::default(), |ui| {
            let mut next_x = Vec::new();
            for active in [false, true] {
                ui.horizontal(|ui| {
                    nav(ui, "Мои задачи · 28", active);
                    next_x.push(ui.next_widget_position().x);
                });
            }
            assert_eq!(next_x[0], next_x[1]);
        });
    }

    #[test]
    fn single_line_fields_and_actions_share_a_height_and_centerline() {
        let ctx = egui::Context::default();
        setup(&ctx);
        let _ = ctx.run_ui(Default::default(), |ui| {
            for enabled in [true, false] {
                ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal(|ui| {
                        let primary = ui.add(primary("Сохранить")).rect;
                        let secondary = ui.button("Отмена").rect;
                        let quiet = ui.add(quiet("Подробнее")).rect;
                        let mut name = String::from("Зимняя акция");
                        let field = ui
                            .add(egui::TextEdit::singleline(&mut name).margin(field_margin(ui)))
                            .rect;
                        for rect in [primary, secondary, quiet, field] {
                            assert!((rect.height() - CONTROL_HEIGHT).abs() <= 0.5, "{rect:?}");
                            assert!((rect.center().y - primary.center().y).abs() <= 0.5);
                        }
                    });
                });
            }
        });
    }
}
