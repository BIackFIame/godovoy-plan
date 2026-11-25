// Модель календаря, которую участники объединяют при синхронизации.
use std::{collections::BTreeMap, sync::Arc};

use chrono::{Datelike, Days, NaiveDate};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type Id = Uuid;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Revision {
    pub sequence: u64,
    // The JSON key is kept for peers through 0.4. This field orders equal
    // sequence numbers, and does not authorize or identify an author.
    #[serde(rename = "device")]
    pub operation: Id,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record<T> {
    pub revision: Revision,
    pub value: T,
}

impl<T> Record<T> {
    pub fn new(revision: Revision, value: T) -> Self {
        Self { revision, value }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Category {
    #[default]
    Campaign,
    Partners,
    Team,
    Operations,
}

impl Category {
    pub const ALL: [Self; 4] = [Self::Campaign, Self::Partners, Self::Team, Self::Operations];

    pub fn label(self) -> &'static str {
        match self {
            Self::Campaign => "Акции",
            Self::Partners => "Партнёры",
            Self::Team => "Команда",
            Self::Operations => "Операции",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: Id,
    pub title: String,
    pub days_before: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub category: Category,
    pub date: NaiveDate,
    pub yearly: bool,
    pub preparation_days: u16,
    pub owner: Option<Id>,
    pub tasks: Vec<Task>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum YearChange {
    UseTemplate,
    Edit(Arc<Event>),
    Cancel,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct YearRecord {
    pub change: Option<Record<YearChange>>,
    pub checks: BTreeMap<Id, Record<bool>>,
    pub note: Option<Record<String>>,
}

impl YearRecord {
    fn merge(&mut self, incoming: Self) {
        merge_optional(&mut self.change, incoming.change);
        merge_optional(&mut self.note, incoming.note);
        merge_map(&mut self.checks, incoming.checks);
    }

    fn latest(&self) -> u64 {
        self.change
            .iter()
            .map(|r| r.revision.sequence)
            .chain(self.note.iter().map(|r| r.revision.sequence))
            .chain(self.checks.values().map(|r| r.revision.sequence))
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSource {
    pub name: String,
    pub external_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ExternalSource>,
    pub templates: BTreeMap<i32, Record<Option<Arc<Event>>>>,
    pub years: BTreeMap<i32, YearRecord>,
}

impl Plan {
    fn has_single_import_history(&self) -> bool {
        self.source.is_some()
            && self
                .templates
                .values()
                .all(|entry| entry.value.as_ref().is_none_or(|event| !event.yearly))
            && self.years.values().all(|entry| {
                !matches!(entry.change.as_ref().map(|record| &record.value),
                    Some(YearChange::Edit(event)) if event.yearly)
            })
    }

    fn single_placement(&self) -> Option<(i32, &Record<Option<Arc<Event>>>)> {
        if !self.has_single_import_history() {
            return None;
        }
        // A move leaves a tombstone and a replacement at the same revision.
        self.templates
            .iter()
            .max_by_key(|&(year, record)| (record.revision, record.value.is_some(), year))
            .map(|(&year, record)| (year, record))
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn import_template(&self) -> Option<(i32, &Event)> {
        if let Some((year, record)) = self.single_placement() {
            return record.value.as_deref().map(|event| (year, event));
        }
        self.templates
            .iter()
            .rev()
            .find_map(|(&year, record)| record.value.as_deref().map(|event| (year, event)))
    }

    pub fn imported_single_has_other_dates(&self) -> bool {
        self.templates.len() > 1 && self.has_single_import_history()
    }

    pub fn event(&self, id: Id, year: i32) -> Option<Occurrence<'_>> {
        let placement = self.single_placement();
        if placement.is_some_and(|(active, record)| active != year || record.value.is_none()) {
            return None;
        }
        let record = self.years.get(&year);
        let change = record.and_then(|r| r.change.as_ref()).filter(|change| {
            // Older writers left no destination change when there was no
            // local override. Ignore a stale override from a parallel branch.
            !placement.is_some_and(|(_, winner)| {
                change.revision < winner.revision
                    && self.templates.iter().any(|(&other_year, other)| {
                        other_year != year
                            && other.value.is_none()
                            && other.revision == winner.revision
                    })
            })
        });
        let (event, revision, overridden) = match change {
            Some(Record {
                value: YearChange::Cancel,
                ..
            }) => return None,
            Some(Record {
                value: YearChange::Edit(event),
                revision,
            }) => (event, *revision, true),
            _ => {
                let (&effective, template) = self.templates.range(..=year).next_back()?;
                let event = template.value.as_ref()?;
                if !event.yearly && year != effective {
                    return None;
                }
                (event, template.revision, false)
            }
        };
        let date = NaiveDate::from_ymd_opt(year, event.date.month(), event.date.day())
            .or_else(|| NaiveDate::from_ymd_opt(year, 2, 28))?;
        Some(Occurrence {
            id,
            year,
            date,
            event,
            revision,
            overridden,
            record,
            single_history: placement.filter(|_| self.templates.len() > 1).map(|_| self),
        })
    }

    fn history(&self) -> impl Iterator<Item = &YearRecord> {
        // A template (including a tombstone) marks years where a single event
        // was scheduled. Recurring occurrences can exist in other years.
        self.templates
            .keys()
            .filter_map(|year| self.years.get(year))
    }

    fn merge(&mut self, incoming: Self) {
        if let Some(source) = incoming.source
            && self.source.as_ref().is_none_or(|current| &source > current)
        {
            self.source = Some(source);
        }
        merge_map(&mut self.templates, incoming.templates);
        for (year, record) in incoming.years {
            self.years.entry(year).or_default().merge(record);
        }
    }

    fn latest(&self) -> u64 {
        self.templates
            .values()
            .map(|r| r.revision.sequence)
            .chain(self.years.values().map(YearRecord::latest))
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
pub struct Occurrence<'a> {
    pub id: Id,
    pub year: i32,
    pub date: NaiveDate,
    pub event: &'a Event,
    pub revision: Revision,
    pub overridden: bool,
    pub record: Option<&'a YearRecord>,
    single_history: Option<&'a Plan>,
}

impl<'a> Occurrence<'a> {
    pub fn preparation_date(self) -> NaiveDate {
        date_before(self.date, self.event.preparation_days)
    }

    pub fn due(self, task: &Task) -> NaiveDate {
        date_before(self.date, task.days_before)
    }

    pub fn checked(self, task: Id) -> bool {
        if let Some(plan) = self.single_history {
            return plan
                .history()
                .filter_map(|year| year.checks.get(&task))
                .max_by_key(|record| record.revision)
                .is_some_and(|record| record.value);
        }
        self.record
            .and_then(|r| r.checks.get(&task))
            .is_some_and(|r| r.value)
    }

    pub fn completed(self) -> usize {
        self.event
            .tasks
            .iter()
            .filter(|task| self.checked(task.id))
            .count()
    }

    pub fn note(self) -> &'a str {
        if let Some(plan) = self.single_history {
            return plan
                .history()
                .filter_map(|year| year.note.as_ref())
                .max_by_key(|record| record.revision)
                .map_or("", |record| record.value.as_str());
        }
        self.record
            .and_then(|r| r.note.as_ref())
            .map_or("", |r| r.value.as_str())
    }

    pub fn needs_attention(self, today: NaiveDate, member: Id) -> bool {
        self.event.owner == Some(member)
            && self.preparation_date() <= today
            && (self.completed() < self.event.tasks.len()
                || (self.event.tasks.is_empty() && self.date >= today))
    }
}

fn date_before(date: NaiveDate, days: u16) -> NaiveDate {
    date.checked_sub_days(Days::new(days.into()))
        .unwrap_or(NaiveDate::MIN)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    pub members: BTreeMap<Id, Record<String>>,
    pub plans: BTreeMap<Id, Plan>,
}

impl Calendar {
    pub fn occurrences(&self, year: i32) -> impl Iterator<Item = Occurrence<'_>> {
        self.plans
            .iter()
            .filter_map(move |(&id, plan)| plan.event(id, year))
    }

    pub fn year(&self, year: i32) -> Vec<Occurrence<'_>> {
        let mut events: Vec<_> = self.occurrences(year).collect();
        events.sort_unstable_by_key(|event| (event.date, event.id));
        events
    }

    pub fn event(&self, id: Id, year: i32) -> Option<Occurrence<'_>> {
        self.plans.get(&id)?.event(id, year)
    }

    /// Follow a moved single import from a former calendar year.
    pub fn follow_event(&self, id: Id, year: i32) -> Option<Occurrence<'_>> {
        let plan = self.plans.get(&id)?;
        plan.event(id, year).or_else(|| {
            if !plan.templates.contains_key(&year) {
                return None;
            }
            let (active, _) = plan.single_placement()?;
            plan.event(id, active)
        })
    }

    pub fn merge(&mut self, incoming: Self) {
        merge_map(&mut self.members, incoming.members);
        for (id, plan) in incoming.plans {
            self.plans.entry(id).or_default().merge(plan);
        }
    }

    pub fn latest(&self) -> u64 {
        self.members
            .values()
            .map(|r| r.revision.sequence)
            .chain(self.plans.values().map(Plan::latest))
            .max()
            .unwrap_or(0)
    }
}

fn merge_optional<T>(current: &mut Option<Record<T>>, incoming: Option<Record<T>>) {
    if let Some(incoming) = incoming
        && current
            .as_ref()
            .is_none_or(|existing| incoming.revision > existing.revision)
    {
        *current = Some(incoming);
    }
}

fn merge_map<K: Ord, T>(current: &mut BTreeMap<K, Record<T>>, incoming: BTreeMap<K, Record<T>>) {
    use std::collections::btree_map::Entry;

    for (key, record) in incoming {
        match current.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(record);
            }
            Entry::Occupied(mut slot) if record.revision > slot.get().revision => {
                slot.insert(record);
            }
            Entry::Occupied(_) => {}
        }
    }
}

pub const MAX_PLANS: usize = 2000;
pub const PLAN_LIMIT_ERROR: &str =
    "Достигнут лимит записей календаря. В него входит история удалений.";
pub const MAX_TASKS: usize = 100;

impl Event {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.title.trim().is_empty() || self.title.chars().count() > 160 {
            return Err("Название события должно содержать от 1 до 160 символов.");
        }
        if !(1900..=9999).contains(&self.date.year())
            || self.description.chars().count() > 4000
            || self.preparation_days > 366
            || self.tasks.len() > MAX_TASKS
        {
            return Err(
                "Превышены ограничения события: год, описание, срок подготовки или число пунктов.",
            );
        }
        let mut ids = std::collections::BTreeSet::new();
        for task in &self.tasks {
            if task.title.trim().is_empty()
                || task.title.chars().count() > 160
                || task.days_before > self.preparation_days
                || !ids.insert(task.id)
            {
                return Err("Проверьте названия, сроки и идентификаторы пунктов чек-листа.");
            }
        }
        Ok(())
    }
}

impl Calendar {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.plans.len() > MAX_PLANS {
            return Err(PLAN_LIMIT_ERROR);
        }
        if self.members.len() > 256
            || self
                .members
                .values()
                .any(|m| m.value.trim().is_empty() || m.value.chars().count() > 80)
        {
            return Err("Превышено ограничение числа участников или длины имени.");
        }
        for plan in self.plans.values() {
            if plan.templates.len() > 150
                || plan.years.len() > 200
                || plan
                    .templates
                    .keys()
                    .chain(plan.years.keys())
                    .any(|y| !(1900..=9999).contains(y))
                || plan
                    .source
                    .as_ref()
                    .is_some_and(|s| s.name.len() > 48 || s.external_id.len() > 160)
            {
                return Err("Превышены ограничения истории события или источника.");
            }
            for template in plan.templates.values() {
                if let Some(event) = &template.value {
                    event.validate()?;
                }
            }
            for year in plan.years.values() {
                if year.checks.len() > MAX_TASKS * 4
                    || year
                        .note
                        .as_ref()
                        .is_some_and(|note| note.value.chars().count() > 4000)
                {
                    return Err("Превышены ограничения чек-листа или дневника.");
                }
                if let Some(Record {
                    value: YearChange::Edit(event),
                    ..
                }) = &year.change
                {
                    event.validate()?;
                }
            }
        }
        Ok(())
    }
}
