//! Voluntary operator-declared metadata and targeted registry operations.
use super::{GrantPurpose, Id, LockedVault, ProtectionConflict};
use crate::Fallible;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

#[derive(Clone, Default, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperatorDetails {
    pub organization: String,
    pub contact: String,
    pub purpose: String,
    pub legal_basis: String,
    pub hosting: String,
    pub backup_location: String,
    pub retention: String,
    pub security_assessment: String,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorRecord {
    pub revision: u64,
    pub details: OperatorDetails,
}
#[derive(Debug)]
pub struct OperatorInput;
impl fmt::Display for OperatorInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Проверьте идентификатор и длину полей. Управляющие символы недопустимы.")
    }
}
impl std::error::Error for OperatorInput {}

impl OperatorDetails {
    fn valid(&self) -> bool {
        [
            &self.organization,
            &self.contact,
            &self.purpose,
            &self.legal_basis,
            &self.hosting,
            &self.backup_location,
            &self.retention,
            &self.security_assessment,
        ]
        .into_iter()
        .all(|s| s.chars().count() <= 1000 && !s.chars().any(|c| c.is_control() && c != '\n'))
    }
}

impl LockedVault<'_> {
    pub fn operator_record(&self) -> Fallible<OperatorRecord> {
        let saved = self.access()?.operator;
        if !saved.details.valid() {
            return Err("Повреждены сохранённые сведения организации.".into());
        }
        Ok(saved)
    }
    pub fn save_operator_record(&self, input: OperatorRecord) -> Fallible<OperatorRecord> {
        if self.standby_window()?.is_some() {
            return Err("Завершите резервный режим перед изменением сведений.".into());
        }
        if !input.details.valid() {
            return Err(OperatorInput.into());
        }
        let mut acl = self.access()?;
        if input.revision != acl.operator.revision {
            return Err(ProtectionConflict.into());
        }
        if input.details != acl.operator.details {
            let revision = input
                .revision
                .checked_add(1)
                .ok_or("Исчерпан номер ревизии.")?;
            acl.operator = OperatorRecord {
                revision,
                details: input.details,
            };
            self.audit_security("operator_details_change_requested", None, None, None)?;
            self.commit_registry(&acl)?;
        }
        Ok(acl.operator)
    }
    pub fn employee_directory(&self) -> Fallible<serde_json::Value> {
        let acl = self.access()?;
        let mut by_employee = BTreeMap::<_, Vec<_>>::new();
        for (id, grant) in &acl.grants {
            by_employee
                .entry(grant.employee)
                .or_default()
                .push((id, grant));
        }
        let employees: Vec<_> = acl.employees.iter().map(|(id, name)| {
            let grants = by_employee.get(id).map(Vec::as_slice).unwrap_or_default();
            let mut mcp_keys = Vec::new();
            let mut parent_rights = Vec::new();
            for &(key_id, grant) in grants {
                match grant.purpose {
                    GrantPurpose::Mcp => mcp_keys.push(serde_json::json!({
                        "id":key_id, "readOnly":grant.read_only, "allowRemote":grant.allow_remote
                    })),
                    GrantPurpose::Employee => parent_rights.push((!grant.read_only,grant.allow_remote)),
                }
            }
            serde_json::json!({
                "id":id, "name":name, "assignedOnly":acl.assigned_only.contains(id), "keys":grants.len(),
                "mcpKeys":mcp_keys, "mcpAvailable":!parent_rights.is_empty(),
                "mcpWrite":parent_rights.iter().any(|r|r.0),
                "mcpRemote":parent_rights.iter().any(|r|r.1),
                "mcpRemoteWrite":parent_rights.contains(&(true,true))
            })
        }).collect();
        self.audit_security("employee_directory_requested", None, None, None)?;
        Ok(serde_json::json!({"employees":employees}))
    }
    pub fn correct_employee_name(&self, id: Id, previous: &str, name: &str) -> Fallible<()> {
        if self.standby_window()?.is_some() {
            return Err("Изменение реестра недоступно в резервном режиме.".into());
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err(OperatorInput.into());
        }
        let mut acl = self.access()?;
        let old = acl.employees.get_mut(&id).ok_or(OperatorInput)?;
        if old != previous {
            return Err(ProtectionConflict.into());
        }
        if old != name {
            *old = name.to_string();
            self.audit_security("employee_name_change_requested", None, Some(id), None)?;
            self.commit_registry(&acl)?;
        }
        Ok(())
    }
}
