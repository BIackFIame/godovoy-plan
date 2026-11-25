use std::f32::consts::{FRAC_PI_2, TAU};

use chrono::{Datelike, NaiveDate};
use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Ui, Vec2, pos2, vec2,
};
use godovoy_plan::model::{Category, Id};

use crate::{
    theme::{ACCENT, ACTION_TEXT, BORDER, INK, MUTED, TINT, heading_font},
    ui::{CalendarRow, event_count, short_date},
};

pub const MONTHS: [&str; 12] = [
    "Янв", "Фев", "Мар", "Апр", "Май", "Июн", "Июл", "Авг", "Сен", "Окт", "Ноя", "Дек",
];

const MONTH_NAMES: [&str; 12] = [
    "Январь",
    "Февраль",
    "Март",
    "Апрель",
    "Май",
    "Июнь",
    "Июль",
    "Август",
    "Сентябрь",
    "Октябрь",
    "Ноябрь",
    "Декабрь",
];

pub fn color(category: Category) -> Color32 {
    match category {
        Category::Campaign => Color32::from_rgb(45, 119, 115),
        Category::Partners => Color32::from_rgb(120, 85, 129),
        Category::Team => Color32::from_rgb(82, 106, 137),
        Category::Operations => Color32::from_rgb(116, 100, 83),
    }
}

pub fn legend_marker(ui: &mut Ui, category: Category) {
    let (rect, _) = ui.allocate_exact_size(vec2(15.0, 15.0), egui::Sense::hover());
    draw_marker(ui.painter(), rect.center(), 5.2, category);
}

pub fn year_wheel(
    ui: &mut Ui,
    max_height: f32,
    year: i32,
    today: NaiveDate,
    events: &[CalendarRow],
    selected: Option<Id>,
) -> Option<Id> {
    let width = ui.available_width();
    if width < 190.0 {
        return None;
    }
    let height = width.min(max_height.max(190.0));
    let (response, painter) = ui.allocate_painter(vec2(width, height), egui::Sense::hover());
    let center = response.rect.center();
    let radius = (height - 59.0) * 0.5;
    let is_current_year = today.year() == year;

    let months = month_slices(events);
    let selected_event = selected.and_then(|id| events.iter().find(|event| event.id == id));
    let context_month = selected_event
        .map(|event| event.date.month0() as usize)
        .or_else(|| is_current_year.then_some(today.month0() as usize));

    for (month, name) in MONTHS.into_iter().enumerate() {
        let current = context_month == Some(month);
        let start = -FRAC_PI_2 + month as f32 * TAU / 12.0;
        let sweep = TAU / 12.0;
        let gap = 3.0 / radius;
        let arc = (0..=12)
            .map(|step| {
                let mark = start + gap + (sweep - 2.0 * gap) * step as f32 / 12.0;
                polar(center, radius, mark)
            })
            .collect();
        painter.add(Shape::line(
            arc,
            Stroke::new(
                if current { 7.0 } else { 4.0 },
                if current { ACCENT } else { BORDER },
            ),
        ));
        if !needs_group(months[month], radius - 12.0) {
            painter.text(
                polar(center, radius + 18.0, start + sweep * 0.5),
                Align2::CENTER_CENTER,
                name,
                if current {
                    heading_font(12.0)
                } else {
                    FontId::proportional(12.0)
                },
                if current { ACTION_TEXT } else { MUTED },
            );
        }
    }
    if is_current_year {
        let mark = angle(today);
        let tick = polar(center, radius + 7.0, mark);
        painter.line_segment([polar(center, radius, mark), tick], Stroke::new(1.5, INK));
        painter.circle_filled(tick, 2.5, INK);
        ui.interact(
            Rect::from_center_size(tick, Vec2::splat(15.0)),
            response.id.with("today"),
            egui::Sense::hover(),
        )
        .on_hover_text(format!("Сегодня · {}", short_date(today)));
    }

    if let Some(month) = context_month {
        painter.text(
            center - vec2(0.0, 7.0),
            Align2::CENTER_CENTER,
            MONTH_NAMES[month],
            heading_font(if height < 235.0 { 20.0 } else { 26.0 }),
            INK,
        );
        painter.text(
            center + vec2(0.0, 14.0),
            Align2::CENTER_CENTER,
            event_count(months[month].len()),
            FontId::proportional(13.0),
            MUTED,
        );
    } else {
        painter.text(
            center,
            Align2::CENTER_CENTER,
            year.to_string(),
            heading_font(22.0),
            INK,
        );
    }

    let mut clicked = None;
    let marker_radius = radius - 12.0;
    for (month, rows) in months.into_iter().enumerate() {
        if needs_group(rows, marker_radius) {
            let rect = month_button(response.rect, radius + 18.0, month);
            let point = rect.center();
            let label = format!("{} · {}", MONTH_NAMES[month], event_count(rows.len()));
            let control = ui
                .interact(
                    rect.expand(1.0),
                    response.id.with(("month", year, month)),
                    egui::Sense::click(),
                )
                .on_hover_text(format!("Показать события · {label}"));
            control.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    format!("Показать события · {label}"),
                )
            });
            let active = selected_event.is_some_and(|e| e.date.month0() as usize == month);
            painter.rect_filled(rect, 6, if active { TINT } else { crate::theme::BACK });
            let count = if rows.len() > 99 {
                "99+".to_owned()
            } else {
                rows.len().to_string()
            };
            painter.text(
                point,
                Align2::CENTER_CENTER,
                format!("{} · {count}", MONTHS[month]),
                heading_font(11.0),
                if active { ACTION_TEXT } else { INK },
            );
            if control.has_focus() || control.hovered() {
                painter.rect_stroke(
                    rect.expand(2.0),
                    7,
                    Stroke::new(1.5, ACCENT),
                    egui::StrokeKind::Outside,
                );
            }
            if let Some(id) = month_menu(&control, rows, selected, label) {
                clicked = Some(id);
            }
            continue;
        }
        for event in rows {
            let point = polar(center, marker_radius, angle(event.date));
            let hit = Rect::from_center_size(point, Vec2::splat(22.0));
            let caption = format!("{} · {}", short_date(event.date), event.title);
            let control = ui
                .interact(hit, response.id.with(event.id), egui::Sense::click())
                .on_hover_text(&caption);
            control.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &caption)
            });
            let active = selected == Some(event.id);
            draw_marker(
                &painter,
                point,
                if active { 6.2 } else { 5.1 },
                event.category,
            );
            if active {
                painter.circle_stroke(point, 9.5, Stroke::new(1.6, ACCENT));
            } else if control.hovered() {
                painter.circle_stroke(point, 8.3, Stroke::new(1.0, MUTED));
            }
            if control.has_focus() {
                painter.rect_stroke(hit, 4, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
            }
            if control.clicked() {
                clicked = Some(event.id);
            }
        }
    }
    clicked
}

fn month_menu(
    control: &egui::Response,
    rows: &[CalendarRow],
    selected: Option<Id>,
    label: String,
) -> Option<Id> {
    const ROW_HEIGHT: f32 = 32.0;
    const MENU_HEIGHT: f32 = 220.0;
    let mut chosen = None;
    egui::Popup::menu(control)
        .width(292.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(276.0);
            ui.spacing_mut().interact_size.y = ROW_HEIGHT;
            ui.label(crate::theme::semibold(label, 14.0, INK));

            let cursor_id = control.id.with("cursor");
            let (mut cursor, offset) = ui
                .data(|data| data.get_temp::<(usize, f32)>(cursor_id))
                .unwrap_or_default();
            let last = rows.len() - 1;
            cursor = cursor.min(last);
            let target = if control.clicked() {
                Some(
                    selected
                        .and_then(|id| rows.iter().position(|row| row.id == id))
                        .unwrap_or(0),
                )
            } else {
                ui.input_mut(|input| {
                    use egui::{Key, Modifiers};
                    if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        Some((cursor + 1).min(last))
                    } else if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        Some(cursor.saturating_sub(1))
                    } else if input.consume_key(Modifiers::NONE, Key::Home) {
                        Some(0)
                    } else if input.consume_key(Modifiers::NONE, Key::End) {
                        Some(last)
                    } else {
                        None
                    }
                })
            };
            let mut scroll = egui::ScrollArea::vertical().max_height(MENU_HEIGHT);
            if let Some(index) = target {
                let top = index as f32 * (ROW_HEIGHT + ui.spacing().item_spacing.y);
                scroll = scroll.vertical_scroll_offset(
                    offset.clamp((top + ROW_HEIGHT - MENU_HEIGHT).max(0.0), top),
                );
            }
            let area = scroll.show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                for index in range {
                    let event = &rows[index];
                    let name = format!("{} · {}", short_date(event.date), event.title);
                    let response = ui
                        .add_sized(
                            vec2(ui.available_width(), ROW_HEIGHT),
                            egui::Button::selectable(selected == Some(event.id), &name).truncate(),
                        )
                        .on_hover_text(name);
                    if target == Some(index) {
                        response.request_focus();
                    }
                    if response.has_focus() {
                        ui.memory_mut(|memory| {
                            memory.set_focus_lock_filter(
                                response.id,
                                egui::EventFilter {
                                    vertical_arrows: true,
                                    ..Default::default()
                                },
                            )
                        });
                        cursor = index;
                    }
                    if response.clicked() {
                        chosen = Some(event.id);
                        ui.close();
                    }
                }
            });
            ui.data_mut(|data| data.insert_temp(cursor_id, (cursor, area.state.offset.y)));
            if chosen.is_some() || ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                control.request_focus();
            }
        });
    chosen
}

pub fn draw_marker(painter: &egui::Painter, point: Pos2, size: f32, category: Category) {
    let ink = color(category);
    match category {
        Category::Campaign => {
            painter.circle_filled(point, size, ink);
        }
        Category::Partners => {
            painter.add(Shape::convex_polygon(
                vec![
                    point + vec2(0.0, -size),
                    point + vec2(size, 0.0),
                    point + vec2(0.0, size),
                    point + vec2(-size, 0.0),
                ],
                ink,
                Stroke::NONE,
            ));
        }
        Category::Team => {
            painter.rect_filled(
                Rect::from_center_size(point, Vec2::splat(size * 1.65)),
                2,
                ink,
            );
        }
        Category::Operations => {
            painter.add(Shape::convex_polygon(
                vec![
                    point + vec2(0.0, -size),
                    point + vec2(size, size * 0.68),
                    point + vec2(-size, size * 0.68),
                ],
                ink,
                Stroke::NONE,
            ));
        }
    }
}

fn angle(date: NaiveDate) -> f32 {
    let days = f32::from(date.num_days_in_month());
    -FRAC_PI_2 + (date.month0() as f32 + (date.day0() as f32 + 0.5) / days) * TAU / 12.0
}

fn month_button(chart: Rect, ring: f32, month: usize) -> Rect {
    let size = vec2(54.0, 22.0);
    let center = chart.center();
    let angle = -FRAC_PI_2 + (month as f32 + 0.5) * TAU / 12.0;
    let mut point = polar(center, ring, angle);
    // Keep the adjacent targets on opposite sides of January and July apart.
    let half_gap = size.x * 0.5 + 2.0;
    point.x = if month < 6 {
        point.x.max(center.x + half_gap)
    } else {
        point.x.min(center.x - half_gap)
    };
    // The next badge on either side is at 45 degrees. Both hit boxes extend 1 pt.
    let outer_row = ring * std::f32::consts::FRAC_1_SQRT_2 + size.y + 2.5;
    match month {
        0 | 11 => point.y = point.y.min(center.y - outer_row),
        5 | 6 => point.y = point.y.max(center.y + outer_row),
        _ => {}
    }
    Rect::from_center_size(chart.shrink2(size * 0.5).clamp(point), size)
}

fn polar(center: Pos2, radius: f32, angle: f32) -> Pos2 {
    pos2(
        center.x + angle.cos() * radius,
        center.y + angle.sin() * radius,
    )
}

// Calendar::year sorts by date; the category and search filters preserve that order.
fn month_slices(mut events: &[CalendarRow]) -> [&[CalendarRow]; 12] {
    std::array::from_fn(|month| {
        let count = events.partition_point(|event| event.date.month0() as usize <= month);
        let (rows, rest) = events.split_at(count);
        events = rest;
        rows
    })
}

fn needs_group(rows: &[CalendarRow], marker_radius: f32) -> bool {
    // Four hit targets cannot fit within one month's arc at our largest dial size.
    rows.len() >= 4
        || rows.windows(2).any(|pair| {
            let gap = angle(pair[1].date) - angle(pair[0].date);
            2.0 * marker_radius * (gap * 0.5).sin() < 22.0
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(month: u32, day: u32, id: u128) -> CalendarRow {
        CalendarRow {
            id: Id::from_u128(id),
            date: NaiveDate::from_ymd_opt(2028, month, day).unwrap(),
            title: String::new(),
            category: Category::Partners,
        }
    }

    #[test]
    fn dense_months_have_separate_hit_boxes_at_every_wheel_width() {
        for width in 190..=368 {
            let height = width.min(308) as f32;
            let chart = Rect::from_min_size(Pos2::ZERO, vec2(width as f32, height));
            let ring = (height - 59.0) * 0.5 + 18.0;
            let buttons: [_; 12] = std::array::from_fn(|month| month_button(chart, ring, month));
            for (i, button) in buttons.iter().enumerate() {
                assert!(chart.contains_rect(*button));
                for other in &buttons[..i] {
                    assert!(!button.expand(1.0).intersects(other.expand(1.0)), "{width}");
                }
            }
        }
    }

    #[test]
    fn dense_month_preserves_every_event_in_date_order() {
        let mut events = vec![event(1, 1, 0)];
        events.extend((1..=125).map(|id| event(2, 29, id)));
        events.push(event(12, 31, 126));
        let months = month_slices(&events);
        assert_eq!(months[0].len(), 1);
        assert_eq!(months[1].len(), 125);
        assert_eq!(months[1][124].id, Id::from_u128(125));
        assert!(months[2..11].iter().all(|month| month.is_empty()));
        assert_eq!(months[11].len(), 1);
        assert!(needs_group(months[1], 115.0));
        assert!(!needs_group(months[0], 115.0));
    }

    #[test]
    fn group_close_dates_when_targets_overlap_at_this_radius() {
        let distant = [event(3, 1, 1), event(3, 15, 2)];
        let adjacent = [event(3, 1, 1), event(3, 2, 2)];
        assert!(!needs_group(&distant, 115.0));
        assert!(needs_group(&distant, 54.0));
        assert!(needs_group(&adjacent, 115.0));
    }
}
