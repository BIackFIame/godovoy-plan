use super::*;

pub(super) fn editor_fields(ui: &mut Ui, editor: &mut Editor, workspace: &Workspace) {
    let calendar = &workspace.calendar;
    let name = ui.label(semibold("Название", 14.0, INK));
    let title_field = ui
        .add(
            TextEdit::singleline(&mut editor.event.title)
                .hint_text("Например, новогодние поздравления")
                .margin(theme::field_margin(ui))
                .desired_width(f32::INFINITY)
                .char_limit(160),
        )
        .labelled_by(name.id);
    if std::mem::take(&mut editor.focus_title) {
        title_field.request_focus();
    }

    ui.horizontal_wrapped(|ui| {
        ui.vertical(|ui| {
            let label = ui.label("Дата события");
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut editor.date_input)
                        .hint_text("ДД.ММ.ГГГГ")
                        .margin(theme::field_margin(ui))
                        .desired_width(100.0)
                        .char_limit(10),
                )
                .labelled_by(label.id);
                let first = editor.existing.map_or(1900, |(year, _)| year);
                let last = editor.existing.map_or(9999, |(year, _)| year);
                let min = NaiveDate::from_ymd_opt(first, 1, 1).unwrap();
                let max = NaiveDate::from_ymd_opt(last, 12, 31).unwrap();
                let selected = editor.parsed_date().unwrap_or(editor.event.date);
                if let Some(date) = date_picker(ui, "Календарь…", selected, min, max) {
                    editor.date_input = date.format("%d.%m.%Y").to_string();
                    editor.error = None;
                }
            });
        });
        ui.vertical(|ui| {
            let label = ui.label("Категория");
            ComboBox::from_id_salt("category")
                .width(113.0)
                .selected_text(editor.event.category.label())
                .show_ui(ui, |ui| {
                    for category in Category::ALL {
                        ui.selectable_value(&mut editor.event.category, category, category.label());
                    }
                })
                .response
                .labelled_by(label.id);
        });
        ui.vertical(|ui| {
            ui.set_width(ui.available_width().clamp(146.0, 216.0));
            let label = ui.label("Ответственный");
            let owner = owner_name(calendar, editor.event.owner);
            let owner = if editor
                .event
                .owner
                .is_some_and(|id| !workspace.person_is_active(id))
            {
                std::borrow::Cow::Owned(format!("{owner} · нет активных устройств"))
            } else {
                std::borrow::Cow::Borrowed(owner)
            };
            ComboBox::from_id_salt("owner")
                .width(ui.available_width())
                .truncate()
                .selected_text(owner.as_ref())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut editor.event.owner, None, "Без ответственного");
                    let selected = editor.event.owner.map(|id| calendar.person_for(id));
                    for (id, label) in employee_options(workspace, false) {
                        if ui.selectable_label(selected == Some(id), label).clicked() {
                            editor.event.owner = Some(id);
                        }
                    }
                })
                .response
                .labelled_by(label.id)
                .on_hover_text(owner.as_ref());
        });
    });
    if let Some((year, _)) = editor.existing {
        ui.horizontal_wrapped(|ui| {
            ui.label("Применить изменения");
            ui.selectable_value(&mut editor.scope, Scope::ThisYear, format!("Только {year}"));
            ui.selectable_value(&mut editor.scope, Scope::Future, format!("С {year}"));
        });
    }
    if editor.scope == Scope::Future {
        ui.checkbox(&mut editor.event.yearly, "Повторять каждый год");
    }

    let event_date = editor
        .parsed_date()
        .ok()
        .filter(|date| (1900..=9999).contains(&date.year()));
    ui.push_id("preparation-date", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("Начало подготовки")
                .on_hover_text("С этого дня чек-лист появляется у ответственного");
            if let Some(date) = event_date {
                let earliest = date - Days::new(366);
                let task_offset = editor
                    .event
                    .tasks
                    .iter()
                    .map(|task| task.days_before)
                    .max()
                    .unwrap_or(0);
                let latest = date - Days::new(task_offset.into());
                let selected = date - Days::new(editor.event.preparation_days.into());
                let label = if editor.event.preparation_days == 0 {
                    "В день события".to_owned()
                } else {
                    deadline_label(selected, date)
                };
                if let Some(start) = date_picker(ui, &label, selected, earliest, latest) {
                    editor.event.preparation_days = days_before(date, start);
                    editor.error = None;
                }
            } else {
                muted(ui, "Укажите дату события");
            }
        });
    });
    ui.collapsing("Описание события", |ui| {
        ui.add(
            TextEdit::multiline(&mut editor.event.description)
                .margin(Margin::same(8))
                .char_limit(4000)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
    });
    ui.separator();
    title(ui, "Чек-лист", SECTION_TITLE_SIZE);
    if editor.event.tasks.is_empty() {
        muted(ui, "Добавьте шаги подготовки и задайте каждому срок.");
    }
    let mut remove = None;
    let focus_last = std::mem::take(&mut editor.focus_new_task);
    let last = editor.event.tasks.len().saturating_sub(1);
    for (index, task) in editor.event.tasks.iter_mut().enumerate() {
        ui.push_id(task.id, |ui| {
            ui.horizontal(|ui| {
                let input = ui.add(
                    TextEdit::singleline(&mut task.title)
                        .id(ui.make_persistent_id("task-title"))
                        .hint_text("Что нужно сделать")
                        .margin(theme::field_margin(ui))
                        .desired_width((ui.available_width() - 218.0).max(120.0))
                        .char_limit(160),
                );
                if focus_last && index == last {
                    input.request_focus();
                    input.scroll_to_me(None);
                }
                if let Some(date) = event_date {
                    let due = date - Days::new(task.days_before.into());
                    let earliest = date - Days::new(366);
                    if let Some(chosen) = date_picker(
                        ui,
                        &format!("До {}", deadline_label(due, date)),
                        due,
                        earliest,
                        date,
                    ) {
                        task.days_before = days_before(date, chosen);
                        editor.event.preparation_days =
                            editor.event.preparation_days.max(task.days_before);
                        editor.error = None;
                    }
                } else {
                    muted(ui, "Срок");
                }
                let remove_task = ui
                    .add(Button::new("×").min_size(Vec2::splat(theme::CONTROL_HEIGHT)))
                    .on_hover_text("Убрать пункт чек-листа");
                remove_task.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        format!("Убрать пункт {}", index + 1),
                    )
                });
                if remove_task.clicked() {
                    remove = Some(task.id);
                }
            });
        });
    }
    if let Some(id) = remove {
        editor.event.tasks.retain(|item| item.id != id);
    }
}

pub(super) fn days_before(event: NaiveDate, selected: NaiveDate) -> u16 {
    (event - selected).num_days().clamp(0, 366) as u16
}
