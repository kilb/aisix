//! How far a stored document has reached the gateways (`Rollout` in the
//! API), and the summary of several documents that a tenant view carries.
//!
//! Gateway heartbeats are not read yet, so no gateway is known to be
//! online: a document is `pending` until the projector has written it to
//! kine, and `no_gateways` after that. Task 9 replaces [`ResourceRollout::of_row`]
//! with the computation over the online instances.

use serde::Serialize;
use serde_json::Value;

use crate::resources::store::ResourceRow;

/// A document's rollout state, as the API names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutState {
    Pending,
    Propagating,
    Applied,
    Rejected,
    NoGateways,
}

impl RolloutState {
    /// How bad the state is when several documents are summarised: a
    /// rejection outranks everything, then a document the gateways cannot
    /// have yet, then one some gateway is still behind on.
    fn severity(self) -> u8 {
        match self {
            RolloutState::Applied => 0,
            RolloutState::NoGateways => 1,
            RolloutState::Propagating => 2,
            RolloutState::Pending => 3,
            RolloutState::Rejected => 4,
        }
    }
}

/// `Rollout = {state, kine_revision, applied, online, rejections}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceRollout {
    pub state: RolloutState,
    pub kine_revision: Option<i64>,
    /// Online gateways that have applied the document.
    pub applied: u64,
    pub online: u64,
    /// `{instance_id, hostname, error}` for each gateway that refused it.
    pub rejections: Vec<Value>,
}

impl ResourceRollout {
    /// The rollout of `row` while no gateway is known to be online.
    pub fn of_row(row: &ResourceRow) -> ResourceRollout {
        let projected = row.projected_version == Some(row.version);
        ResourceRollout {
            state: if projected {
                RolloutState::NoGateways
            } else {
                RolloutState::Pending
            },
            kine_revision: row.kine_revision,
            applied: 0,
            online: 0,
            rejections: Vec::new(),
        }
    }
}

/// The summary of `states`: the worst of them (the first, among equals),
/// with every document's rejections, the fewest gateways any document has
/// reached, and the most gateways seen online. Nothing to summarise is
/// nothing left to roll out: `applied`.
pub fn worst(states: &[ResourceRollout]) -> ResourceRollout {
    let Some(first) = states.first() else {
        return ResourceRollout {
            state: RolloutState::Applied,
            kine_revision: None,
            applied: 0,
            online: 0,
            rejections: Vec::new(),
        };
    };
    let mut summary = states
        .iter()
        .fold(first, |worst, s| {
            if s.state.severity() > worst.state.severity() {
                s
            } else {
                worst
            }
        })
        .clone();
    summary.applied = states.iter().map(|s| s.applied).min().unwrap_or(0);
    summary.online = states.iter().map(|s| s.online).max().unwrap_or(0);
    summary.rejections = states
        .iter()
        .flat_map(|s| s.rejections.iter().cloned())
        .collect();
    summary
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn at(state: RolloutState, applied: u64, online: u64) -> ResourceRollout {
        ResourceRollout {
            state,
            kine_revision: Some(applied as i64),
            applied,
            online,
            rejections: Vec::new(),
        }
    }

    #[test]
    fn the_worst_state_wins_whatever_the_order() {
        use RolloutState::*;
        let ranked = [Applied, NoGateways, Propagating, Pending, Rejected];
        for (i, worse) in ranked.iter().enumerate() {
            for better in &ranked[..i] {
                for pair in [
                    [at(*better, 1, 1), at(*worse, 1, 1)],
                    [at(*worse, 1, 1), at(*better, 1, 1)],
                ] {
                    assert_eq!(worst(&pair).state, *worse, "{better:?} vs {worse:?}");
                }
            }
        }
    }

    #[test]
    fn a_summary_keeps_every_rejection_and_the_least_progress() {
        let mut rejected = at(RolloutState::Rejected, 3, 4);
        rejected.rejections = vec![json!({"instance_id": "gw-a-7c1"})];
        let mut also = at(RolloutState::Rejected, 2, 4);
        also.rejections = vec![json!({"instance_id": "gw-b-7c1"})];
        let summary = worst(&[at(RolloutState::Applied, 4, 4), rejected, also]);
        assert_eq!(summary.state, RolloutState::Rejected);
        assert_eq!(summary.kine_revision, Some(3), "the first of the worst");
        assert_eq!((summary.applied, summary.online), (2, 4));
        assert_eq!(
            summary.rejections,
            [
                json!({"instance_id": "gw-a-7c1"}),
                json!({"instance_id": "gw-b-7c1"})
            ]
        );
        assert_eq!(worst(&[]).state, RolloutState::Applied);
    }

    #[test]
    fn the_states_are_named_as_the_api_names_them() {
        let names: Vec<Value> = [
            RolloutState::Pending,
            RolloutState::Propagating,
            RolloutState::Applied,
            RolloutState::Rejected,
            RolloutState::NoGateways,
        ]
        .iter()
        .map(|s| serde_json::to_value(s).expect("serialize"))
        .collect();
        assert_eq!(
            names,
            [
                json!("pending"),
                json!("propagating"),
                json!("applied"),
                json!("rejected"),
                json!("no_gateways")
            ]
        );
    }
}
