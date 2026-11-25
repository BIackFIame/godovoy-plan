use serde::{Deserialize, Serialize};

pub const BATCH_LIMIT: usize = 100;
pub const BRIDGE_RUN_LIMIT: u16 = 2000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeRun {
    pub fetched: u16,
    pub confirmed: u16,
    pub changed: u16,
    pub outcome: BridgeOutcome,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum BridgeOutcome {
    Complete,
    SourceUnavailable,
    SourceRejected,
    SourceInvalid,
    BatchRejected,
    ApiUnavailable,
    ConfirmationLost,
}

impl BridgeRun {
    pub(crate) fn valid(self) -> bool {
        self.fetched <= BRIDGE_RUN_LIMIT
            && self.changed <= self.confirmed
            && self.confirmed <= self.fetched
            && match self.outcome {
                BridgeOutcome::Complete => self.confirmed == self.fetched,
                BridgeOutcome::SourceUnavailable
                | BridgeOutcome::SourceRejected
                | BridgeOutcome::SourceInvalid => self.fetched == 0,
                BridgeOutcome::BatchRejected
                | BridgeOutcome::ApiUnavailable
                | BridgeOutcome::ConfirmationLost => self.confirmed < self.fetched,
            }
    }
}
