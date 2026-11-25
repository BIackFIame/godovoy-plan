use super::{ComboBox, Fallible, Id, RED, Ui, Workspace, egui, muted};

/// Offer one choice per employee, including historical employees in reports.
pub(super) fn employee_options(workspace: &Workspace, history: bool) -> Vec<(Id, String)> {
    let mut people = workspace.employees();
    if history {
        for &member in workspace.calendar.members.keys() {
            let person = workspace.calendar.person_for(member);
            people.entry(person).or_insert_with(|| {
                workspace
                    .calendar
                    .person_name(person)
                    .unwrap_or("Неизвестный сотрудник")
                    .to_owned()
            });
        }
    }
    let mut people: Vec<_> = people.into_iter().collect();
    people.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    for namesakes in people.chunk_by_mut(|first, next| first.1 == next.1) {
        if namesakes.len() > 1 {
            for (id, name) in namesakes {
                *name = format!("{name} · {}", &id.to_string()[..8]);
            }
        }
    }
    people
}

#[derive(Default)]
pub(super) struct DeviceAssignments {
    choice: Option<Choice>,
    status: Option<(String, bool)>,
}

struct Choice {
    device: Id,
    original: Id,
    selected: Id,
}

impl DeviceAssignments {
    pub(super) fn show(
        &mut self,
        ui: &mut Ui,
        workspace: &Workspace,
        local: Id,
        busy: bool,
    ) -> Option<(Id, Id)> {
        let calendar = &workspace.calendar;
        muted(
            ui,
            "Подключите каждое устройство по своему приглашению. Затем укажите, чьи задачи и напоминания на нём показывать.",
        );
        let device_label = |id: Id| {
            let name = calendar.members.get(&id).map_or("", |m| m.value.as_str());
            let here = if id == local {
                " · это устройство"
            } else {
                ""
            };
            format!("{name}{here} · {}", &id.to_string()[..8])
        };
        if let Some(choice) = &self.choice {
            if workspace.is_revoked(choice.device) || !calendar.members.contains_key(&choice.device)
            {
                self.choice = None;
                self.status = None;
            } else if calendar.person_for(choice.device) != choice.original {
                self.choice = None;
                self.status = Some((
                    "Назначение устройства изменилось. Выберите его заново.".into(),
                    false,
                ));
            }
        }
        let label = ui.label("Устройство");
        ComboBox::from_id_salt("employee-device")
            .selected_text(
                self.choice
                    .as_ref()
                    .map_or_else(|| "Выберите устройство".into(), |c| device_label(c.device)),
            )
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for &id in calendar
                    .members
                    .keys()
                    .filter(|id| !workspace.is_revoked(**id))
                {
                    let selected = self.choice.as_ref().is_some_and(|c| c.device == id);
                    if ui
                        .selectable_label(selected, device_label(id))
                        .on_hover_text(id.to_string())
                        .clicked()
                    {
                        let original = calendar.person_for(id);
                        self.choice = Some(Choice {
                            device: id,
                            original,
                            selected: original,
                        });
                        self.status = None;
                    }
                }
            })
            .response
            .labelled_by(label.id);
        let mut change = None;
        if let Some(choice) = &mut self.choice {
            muted(ui, format!("ID устройства · {}", choice.device));
            let label = ui.label("Сотрудник");
            let detach = choice.selected == choice.device && choice.original != choice.device;
            let options = employee_options(workspace, false);
            let selected = if detach {
                "Отдельный сотрудник"
            } else {
                options
                    .iter()
                    .find(|(id, _)| *id == choice.selected)
                    .map_or("Выберите сотрудника", |(_, name)| {
                        name.as_str()
                    })
            };
            ComboBox::from_id_salt("device-employee")
                .selected_text(selected)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for (id, name) in &options {
                        ui.selectable_value(&mut choice.selected, *id, name)
                            .on_hover_text(id.to_string());
                    }
                    if choice.original != choice.device {
                        ui.separator();
                        ui.selectable_value(
                            &mut choice.selected,
                            choice.device,
                            "Отдельный сотрудник",
                        );
                    }
                })
                .response
                .labelled_by(label.id);
            if ui
                .add_enabled(
                    !busy && choice.original != choice.selected,
                    egui::Button::new("Сохранить назначение"),
                )
                .clicked()
            {
                change = Some((choice.device, choice.selected));
            }
        }
        if let Some((message, error)) = &self.status {
            if *error {
                ui.colored_label(RED, message);
            } else {
                muted(ui, message);
            }
        }
        change
    }

    pub(super) fn finish(&mut self, result: Fallible<()>) {
        match result {
            Ok(()) => {
                if let Some(choice) = &mut self.choice {
                    choice.original = choice.selected;
                }
                self.status = Some(("Назначение устройства сохранено.".into(), false));
            }
            Err(error) => self.status = Some((error.to_string(), true)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use godovoy_plan::{model::Record, storage::Store};

    #[test]
    fn owner_picker_groups_devices_but_distinguishes_namesakes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("team")).unwrap();
        let mut store = store.lock().unwrap();
        let person = store.state().device;
        let colleague = Id::new_v4();
        let phone = Id::new_v4();
        store
            .transaction(|s| {
                s.create_workspace("Офис".into(), "Анна".into())?;
                let revision = s.revision()?;
                let calendar = &mut s.workspace_mut()?.calendar;
                calendar
                    .members
                    .insert(colleague, Record::new(revision, "Анна".into()));
                calendar
                    .members
                    .insert(phone, Record::new(revision, "Телефон".into()));
                Ok(())
            })
            .unwrap();
        store.link_employee_device(phone, person).unwrap();
        let workspace = store.state().workspace.as_ref().unwrap();
        for historical in [false, true] {
            let options = employee_options(workspace, historical);
            assert_eq!(options.len(), 2);
            assert!(options.iter().any(|entry| entry.0 == person));
            assert!(options.iter().any(|entry| entry.0 == colleague));
            assert_ne!(options[0].1, options[1].1);
        }
        assert_eq!(
            super::super::owner_name(&workspace.calendar, Some(phone)),
            "Анна"
        );
    }
}
