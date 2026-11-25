//! Local observations for the administrator. File coverage does not establish who operated the system.
use super::{ACCESS, LockedVault, snapshot};
use crate::Fallible;
use chrono::{Datelike, NaiveDate, Utc};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, io::ErrorKind};

pub(super) const RECEIPT: &str = "online-backup-check-v1.json";

pub(super) fn audit_month(name: &str) -> Option<NaiveDate> {
    let month = name.strip_prefix("security-")?.strip_suffix(".jsonl")?;
    if month.len() != 7 || month.as_bytes()[4] != b'-' {
        return None;
    }
    let year = month.get(..4)?;
    let month = month.get(5..)?;
    if !year
        .bytes()
        .chain(month.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, 1)
}

fn month_index(date: NaiveDate) -> i32 {
    date.year() * 12 + date.month0() as i32
}

fn label(month: i32) -> String {
    format!(
        "{:04}-{:02}",
        month.div_euclid(12),
        month.rem_euclid(12) + 1
    )
}

fn year_window(today: NaiveDate, months: &BTreeSet<i32>) -> Value {
    // The same date a year ago and today can each cover only part of their month.
    let end = month_index(today);
    let start = end - 12;
    let missing: Vec<_> = (start..=end)
        .filter(|n| !months.contains(n))
        .map(label)
        .collect();
    json!({"fromMonth":label(start), "toMonth":label(end), "missingMonths":missing})
}

fn regulatory_follow_up(settings: super::ProtectionSettings) -> Vec<Value> {
    let mut groups = Vec::new();
    if settings.russia {
        groups.push(json!({
            "jurisdiction":"RU", "law":"Федеральный закон № 152-ФЗ",
            "checks":[
                "Ст. 18 ч. 5. Проверьте размещение баз, используемых при сборе данных граждан РФ, и применимость предусмотренных исключений.",
                "Ст. 19. Определите угрозы и уровень защищённости системы, зафиксируйте меры защиты и результаты проверки их работы.",
                "Ст. 21. Установите порядок уточнения, блокирования и уничтожения данных. Для инцидентов по ч. 3.1 организуйте уведомление Роскомнадзора в течение 24 часов и сообщение результатов расследования в течение 72 часов с момента выявления.",
                "Ст. 22. Определите обязанность уведомить Роскомнадзор до начала обработки и зафиксируйте применимое основание для исключения, если оно есть."
            ],
            "sources":["https://mintrud.gov.ru/docs/laws/130"]
        }));
    }
    if settings.belarus {
        groups.push(json!({
            "jurisdiction":"BY", "law":"Закон Республики Беларусь № 99-З и приказ ОАЦ № 66",
            "checks":[
                "Закон № 99-З. Определите основания и сроки обработки, порядок выполнения запросов субъектов, условия трансграничной передачи и ответственных за меры защиты.",
                "Приказ № 66. Определите класс информационной системы и применимые требования. По п. 1.2 приложения 3 проверьте сбор и сохранность сведений о событиях информационной безопасности за установленный срок, минимум один год.",
                "Приложение 3, пп. 7.5–7.6. Утвердите состав копируемой информации и обеспечьте резервное копирование. Сверьте перечень компании с содержимым фактической копии."
            ],
            "sources":["https://etalonline.by/document/?regnum=H12100099", "https://etalonline.by/document/?regnum=T62004470"]
        }));
    }
    groups
}

impl LockedVault<'_> {
    pub fn protection_report(&self) -> Fallible<Value> {
        snapshot::validate_evidence_directory(self.directory)?;
        snapshot::private_file(&self.directory.join(ACCESS))?;
        let acl = self.access()?;
        let policy = acl.protection();
        let mut months = BTreeSet::new();
        for entry in fs::read_dir(self.directory)? {
            let entry = entry?;
            if let Some(month) = entry.file_name().to_str().and_then(audit_month)
                && snapshot::private_file(&entry.path())?.metadata()?.len() != 0
            {
                months.insert(month_index(month));
            }
        }
        let mut gaps = Vec::new();
        let mut previous = None;
        for &next in &months {
            if let Some(before) = previous
                && next > before + 1
            {
                gaps.push(json!({"from":label(before + 1),"to":label(next - 1)}));
            }
            previous = Some(next);
        }
        let now = Utc::now();
        let remote = acl.grants.values().filter(|g| g.allow_remote).count();
        let receipt = match fs::symlink_metadata(self.directory.join(RECEIPT)) {
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
            Ok(_) => {
                snapshot::private_file(&self.directory.join(RECEIPT))?;
                Some(self.read::<chrono::DateTime<Utc>>(RECEIPT, 128)?)
            }
        };
        Ok(json!({
            "schemaVersion":1, "generatedAt": now, "serverVersion": env!("CARGO_PKG_VERSION"),
            "protection": policy,
            "effective":{"remoteAllowed":policy.settings.remote_allowed(), "freeTextAllowed":policy.settings.free_text_allowed()},
            "credentials":{"active":acl.grants.len(),"officeOnly":acl.grants.len()-remote, "remoteEnabled":remote,
                "readOnly":acl.grants.values().filter(|g| g.read_only).count(),
                "assignedOnly":acl.grants.values().filter(|g| acl.assigned_only.contains(&g.employee)).count()},
            "storage":{"directoryChecked":true,"accessFileChecked":true, "platform":std::env::consts::OS},
            "audit":{"months":months.len(),"oldestMonth":months.first().copied().map(label),
                "latestMonth":months.last().copied().map(label),"gaps":gaps,"coverageBasis":"nonemptyMonthlyFiles",
                "yearWindow":year_window(now.date_naive(), &months),
                "coverageNote":"Непустой месячный файл показывает присутствие журнала. Полноту регистрации и сохранность каждого обязательного события проверяет оператор."},
            "backup":{"lastVerifiedAt":receipt},
            "operatorProvided":self.operator_record()?,
            "regulatoryFollowUp":regulatory_follow_up(policy.settings),
            "operatorChecks":["Страна размещения сервера, копий и устройств","Правовые основания и сроки хранения",
                "Категории данных, оценка угроз и применимые требования","Настройки внешнего TLS, сети и резервной площадки",
                "Доступ к носителям и действующая копия вне основного узла"]
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_period_includes_both_boundary_months_at_leap_day() {
        let today = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let end = month_index(today);
        let all: BTreeSet<_> = (end - 12..=end).collect();
        let window = year_window(today, &all);
        assert_eq!(
            window,
            json!({"fromMonth":"2023-02","toMonth":"2024-02","missingMonths":[]})
        );
        let partial: BTreeSet<_> = (end - 11..end).collect();
        assert_eq!(
            year_window(today, &partial)["missingMonths"],
            json!(["2023-02", "2024-02"])
        );
        assert_eq!(
            year_window(today, &BTreeSet::new())["missingMonths"]
                .as_array()
                .unwrap()
                .len(),
            13
        );
    }

    #[test]
    fn regulatory_items_follow_only_selected_jurisdictions() {
        let mut settings = super::super::ProtectionSettings::default();
        assert!(regulatory_follow_up(settings).is_empty());
        settings.belarus = true;
        assert_eq!(regulatory_follow_up(settings)[0]["jurisdiction"], "BY");
        settings.russia = true;
        assert_eq!(regulatory_follow_up(settings).len(), 2);
        settings.belarus = false;
        assert_eq!(regulatory_follow_up(settings)[0]["jurisdiction"], "RU");
    }
}
