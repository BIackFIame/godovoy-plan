use super::*;

#[derive(Default)]
pub(super) struct HostedEntry {
    address: String,
    invalid: bool,
}

impl HostedEntry {
    pub(super) fn show(&mut self, ui: &mut Ui) {
        ui.collapsing("Серверный календарь", |ui| {
            muted(ui, "Отдельный календарь компании работает в браузере, пока доступен сервер. События с этого устройства автоматически не переносятся.");
            let label = ui.label("Адрес от администратора");
            let field = ui.add(TextEdit::singleline(&mut self.address)
                .hint_text("https://calendar.example.org").char_limit(300)
                .desired_width(f32::INFINITY)).labelled_by(label.id);
            if field.changed() { self.invalid = false; }
            if ui.add_enabled(!self.address.trim().is_empty(), quiet("Открыть в браузере")).clicked() {
                let url = self.address.trim();
                if godovoy_plan::hosted_address::valid_hosted_address(url) {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    self.address.clear();
                    self.invalid = false;
                } else {
                    self.invalid = true;
                }
            }
            if self.invalid {
                ui.colored_label(RED, "Нужен HTTPS-адрес без пути, кода и параметров. Для туннеля используйте http://127.0.0.1:45838.");
            }
        });
    }
}
