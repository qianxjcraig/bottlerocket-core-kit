use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::fmt;

/// A complete accelerator configuration qualified for one workload class.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcceleratorProfile {
    /// MIG-partitioned GPUs allocated through Kubernetes DRA.
    SharedInference,
    /// Full GPUs allocated through Kubernetes DRA after NCCL/EFA qualification.
    DistributedTraining,
}

/// Identifies one transition and prevents stale requests from replaying across generations.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionId {
    generation: u64,
    token: String,
}

impl TransitionId {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Display for TransitionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.generation, self.token)
    }
}

/// The persisted step reached by an active profile transition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransitionPhase {
    IntentPersisted,
    NodeDrained,
    AdvertisementWithdrawn,
    TargetProfileRebootRequired,
    TargetProfileApplied,
    TargetDraValidated,
    TargetQualified,
    RestorePending,
    PreviousProfileRebootRequired,
    PreviousProfileApplied,
    PreviousDraValidated,
    Blocked,
}

/// The next operation owned by either the node service or cluster coordinator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NextAction {
    CoordinatorDrainNode,
    NodeWithdrawAdvertisement,
    NodeApplyTargetProfile,
    NodeValidateTargetDra,
    CoordinatorRunQualification,
    NodeCommitTarget,
    NodeApplyPreviousProfile,
    NodeValidatePreviousDra,
    NodeCommitRestore,
    ManualIntervention,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ActiveTransition {
    id: TransitionId,
    previous_profile: Option<AcceleratorProfile>,
    target_profile: AcceleratorProfile,
    phase: TransitionPhase,
    failure_reason: Option<String>,
}

/// The result retained after an active transition is committed or restored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionResult {
    id: TransitionId,
    previous_profile: Option<AcceleratorProfile>,
    target_profile: AcceleratorProfile,
    outcome: TransitionOutcome,
    failure_reason: Option<String>,
}

impl TransitionResult {
    pub fn id(&self) -> &TransitionId {
        &self.id
    }

    pub fn previous_profile(&self) -> Option<AcceleratorProfile> {
        self.previous_profile
    }

    pub fn target_profile(&self) -> AcceleratorProfile {
        self.target_profile
    }

    pub fn outcome(&self) -> TransitionOutcome {
        self.outcome
    }

    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }
}

/// Final disposition of a completed transition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransitionOutcome {
    TargetCommitted,
    PreviousProfileRestored,
}

/// Durable state owned by the accelerator node lifecycle service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleState {
    #[serde(default = "current_schema_version")]
    schema_version: u32,
    generation: u64,
    committed_profile: Option<AcceleratorProfile>,
    active_transition: Option<ActiveTransition>,
    last_result: Option<TransitionResult>,
}

impl Default for LifecycleState {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LifecycleState {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn new(committed_profile: Option<AcceleratorProfile>) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            generation: 0,
            committed_profile,
            active_transition: None,
            last_result: None,
        }
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn committed_profile(&self) -> Option<AcceleratorProfile> {
        self.committed_profile
    }

    pub fn active_transition_id(&self) -> Option<&TransitionId> {
        self.active_transition
            .as_ref()
            .map(|transition| &transition.id)
    }

    pub fn target_profile(&self) -> Option<AcceleratorProfile> {
        self.active_transition
            .as_ref()
            .map(|transition| transition.target_profile)
    }

    pub fn previous_profile(&self) -> Option<AcceleratorProfile> {
        self.active_transition
            .as_ref()
            .and_then(|transition| transition.previous_profile)
    }

    pub fn phase(&self) -> Option<TransitionPhase> {
        self.active_transition
            .as_ref()
            .map(|transition| transition.phase)
    }

    pub fn failure_reason(&self) -> Option<&str> {
        self.active_transition
            .as_ref()
            .and_then(|transition| transition.failure_reason.as_deref())
    }

    pub fn last_result(&self) -> Option<&TransitionResult> {
        self.last_result.as_ref()
    }

    /// Starts a transition only if the caller observed the current generation.
    ///
    /// Retrying the same token, target, and observed generation returns the original ID. Once a
    /// newer generation exists, the old request is rejected instead of replayed.
    pub fn begin(
        &mut self,
        token: impl Into<String>,
        target_profile: AcceleratorProfile,
        expected_generation: u64,
    ) -> Result<TransitionId, LifecycleError> {
        let token = token.into();
        validate_transition_token(&token)?;
        let requested_generation = expected_generation
            .checked_add(1)
            .ok_or(LifecycleError::GenerationExhausted)?;

        if let Some(transition) = &self.active_transition {
            if transition.id.token == token
                && transition.id.generation == requested_generation
                && transition.target_profile == target_profile
            {
                return Ok(transition.id.clone());
            }
            return ActiveTransitionExistsSnafu {
                active_id: transition.id.clone(),
            }
            .fail();
        }

        if let Some(result) = self
            .last_result
            .as_ref()
            .filter(|result| result.id.token == token)
        {
            if result.id.generation == requested_generation
                && result.outcome == TransitionOutcome::TargetCommitted
                && result.target_profile == target_profile
            {
                return Ok(result.id.clone());
            }
            return TransitionTokenAlreadyUsedSnafu {
                token,
                generation: result.id.generation,
                outcome: result.outcome,
            }
            .fail();
        }

        if expected_generation != self.generation {
            return StaleGenerationSnafu {
                expected: self.generation,
                actual: expected_generation,
            }
            .fail();
        }

        if self.committed_profile == Some(target_profile) {
            return ProfileAlreadyCommittedSnafu { target_profile }.fail();
        }

        let id = TransitionId {
            generation: requested_generation,
            token,
        };
        self.generation = requested_generation;
        self.active_transition = Some(ActiveTransition {
            id: id.clone(),
            previous_profile: self.committed_profile,
            target_profile,
            phase: TransitionPhase::IntentPersisted,
            failure_reason: None,
        });
        Ok(id)
    }

    pub fn record_node_drained(&mut self, id: &TransitionId) -> Result<(), LifecycleError> {
        self.advance(
            id,
            TransitionPhase::IntentPersisted,
            TransitionPhase::NodeDrained,
        )
    }

    pub fn record_advertisement_withdrawn(
        &mut self,
        id: &TransitionId,
    ) -> Result<(), LifecycleError> {
        self.advance(
            id,
            TransitionPhase::NodeDrained,
            TransitionPhase::AdvertisementWithdrawn,
        )
    }

    pub fn record_target_profile_reboot_required(
        &mut self,
        id: &TransitionId,
    ) -> Result<(), LifecycleError> {
        self.advance(
            id,
            TransitionPhase::AdvertisementWithdrawn,
            TransitionPhase::TargetProfileRebootRequired,
        )
    }

    pub fn record_target_profile_applied(
        &mut self,
        id: &TransitionId,
    ) -> Result<(), LifecycleError> {
        self.advance_from(
            id,
            &[
                TransitionPhase::AdvertisementWithdrawn,
                TransitionPhase::TargetProfileRebootRequired,
            ],
            TransitionPhase::TargetProfileApplied,
        )
    }

    pub fn record_target_dra_validated(&mut self, id: &TransitionId) -> Result<(), LifecycleError> {
        self.advance(
            id,
            TransitionPhase::TargetProfileApplied,
            TransitionPhase::TargetDraValidated,
        )
    }

    pub fn record_target_qualified(&mut self, id: &TransitionId) -> Result<(), LifecycleError> {
        self.advance(
            id,
            TransitionPhase::TargetDraValidated,
            TransitionPhase::TargetQualified,
        )
    }

    pub fn commit_target(&mut self, id: &TransitionId) -> Result<(), LifecycleError> {
        if self.active_transition.is_none() {
            if self.completed_with(id, TransitionOutcome::TargetCommitted) {
                return Ok(());
            }
            return NoActiveTransitionSnafu.fail();
        }

        self.ensure_phase(id, TransitionPhase::TargetQualified)?;
        let transition = self
            .active_transition
            .take()
            .expect("transition checked above");
        self.committed_profile = Some(transition.target_profile);
        self.last_result = Some(TransitionResult {
            id: transition.id,
            previous_profile: transition.previous_profile,
            target_profile: transition.target_profile,
            outcome: TransitionOutcome::TargetCommitted,
            failure_reason: None,
        });
        Ok(())
    }

    pub fn request_restore(
        &mut self,
        id: &TransitionId,
        failure_reason: impl Into<String>,
    ) -> Result<(), LifecycleError> {
        let failure_reason = failure_reason.into();
        if failure_reason.trim().is_empty() {
            return EmptyFailureReasonSnafu.fail();
        }

        if self.active_transition.is_none() {
            if self.completed_with(id, TransitionOutcome::PreviousProfileRestored) {
                return Ok(());
            }
            return NoActiveTransitionSnafu.fail();
        }

        let transition = self.transition_mut(id)?;
        match transition.phase {
            TransitionPhase::RestorePending
            | TransitionPhase::PreviousProfileRebootRequired
            | TransitionPhase::PreviousProfileApplied
            | TransitionPhase::PreviousDraValidated
            | TransitionPhase::Blocked => {
                if transition.failure_reason.as_deref() == Some(failure_reason.as_str()) {
                    return Ok(());
                }
                return RestoreAlreadyRequestedSnafu {
                    phase: transition.phase,
                }
                .fail();
            }
            _ => {}
        }

        transition.failure_reason = Some(failure_reason);
        transition.phase = if transition.previous_profile.is_some() {
            TransitionPhase::RestorePending
        } else {
            TransitionPhase::Blocked
        };
        Ok(())
    }

    pub fn record_previous_profile_reboot_required(
        &mut self,
        id: &TransitionId,
    ) -> Result<(), LifecycleError> {
        self.advance(
            id,
            TransitionPhase::RestorePending,
            TransitionPhase::PreviousProfileRebootRequired,
        )
    }

    pub fn record_previous_profile_applied(
        &mut self,
        id: &TransitionId,
    ) -> Result<(), LifecycleError> {
        self.advance_from(
            id,
            &[
                TransitionPhase::RestorePending,
                TransitionPhase::PreviousProfileRebootRequired,
            ],
            TransitionPhase::PreviousProfileApplied,
        )
    }

    pub fn record_previous_dra_validated(
        &mut self,
        id: &TransitionId,
    ) -> Result<(), LifecycleError> {
        self.advance(
            id,
            TransitionPhase::PreviousProfileApplied,
            TransitionPhase::PreviousDraValidated,
        )
    }

    pub fn commit_restore(&mut self, id: &TransitionId) -> Result<(), LifecycleError> {
        if self.active_transition.is_none() {
            if self.completed_with(id, TransitionOutcome::PreviousProfileRestored) {
                return Ok(());
            }
            return NoActiveTransitionSnafu.fail();
        }

        self.ensure_phase(id, TransitionPhase::PreviousDraValidated)?;
        let transition = self
            .active_transition
            .take()
            .expect("transition checked above");
        self.committed_profile = transition.previous_profile;
        self.last_result = Some(TransitionResult {
            id: transition.id,
            previous_profile: transition.previous_profile,
            target_profile: transition.target_profile,
            outcome: TransitionOutcome::PreviousProfileRestored,
            failure_reason: transition.failure_reason,
        });
        Ok(())
    }

    pub fn next_action(&self) -> Option<NextAction> {
        let phase = self.active_transition.as_ref()?.phase;
        Some(match phase {
            TransitionPhase::IntentPersisted => NextAction::CoordinatorDrainNode,
            TransitionPhase::NodeDrained => NextAction::NodeWithdrawAdvertisement,
            TransitionPhase::AdvertisementWithdrawn => NextAction::NodeApplyTargetProfile,
            TransitionPhase::TargetProfileRebootRequired => {
                NextAction::NodeApplyTargetProfile
            }
            TransitionPhase::TargetProfileApplied => NextAction::NodeValidateTargetDra,
            TransitionPhase::TargetDraValidated => NextAction::CoordinatorRunQualification,
            TransitionPhase::TargetQualified => NextAction::NodeCommitTarget,
            TransitionPhase::RestorePending => NextAction::NodeApplyPreviousProfile,
            TransitionPhase::PreviousProfileRebootRequired => NextAction::NodeApplyPreviousProfile,
            TransitionPhase::PreviousProfileApplied => NextAction::NodeValidatePreviousDra,
            TransitionPhase::PreviousDraValidated => NextAction::NodeCommitRestore,
            TransitionPhase::Blocked => NextAction::ManualIntervention,
        })
    }

    fn completed_with(&self, id: &TransitionId, outcome: TransitionOutcome) -> bool {
        self.last_result
            .as_ref()
            .is_some_and(|result| &result.id == id && result.outcome == outcome)
    }

    fn transition_mut(
        &mut self,
        id: &TransitionId,
    ) -> Result<&mut ActiveTransition, LifecycleError> {
        let transition = self
            .active_transition
            .as_mut()
            .ok_or(LifecycleError::NoActiveTransition)?;
        if &transition.id != id {
            return TransitionIdMismatchSnafu {
                expected: transition.id.clone(),
                actual: id.clone(),
            }
            .fail();
        }
        Ok(transition)
    }

    fn ensure_phase(
        &self,
        id: &TransitionId,
        expected: TransitionPhase,
    ) -> Result<(), LifecycleError> {
        let transition = self
            .active_transition
            .as_ref()
            .ok_or(LifecycleError::NoActiveTransition)?;
        if &transition.id != id {
            return TransitionIdMismatchSnafu {
                expected: transition.id.clone(),
                actual: id.clone(),
            }
            .fail();
        }
        if transition.phase != expected {
            return UnexpectedPhaseSnafu {
                expected,
                actual: transition.phase,
            }
            .fail();
        }
        Ok(())
    }

    fn advance(
        &mut self,
        id: &TransitionId,
        expected: TransitionPhase,
        next: TransitionPhase,
    ) -> Result<(), LifecycleError> {
        let transition = self.transition_mut(id)?;
        if transition.phase == next {
            return Ok(());
        }
        if transition.phase != expected {
            return UnexpectedPhaseSnafu {
                expected,
                actual: transition.phase,
            }
            .fail();
        }
        transition.phase = next;
        Ok(())
    }

    fn advance_from(
        &mut self,
        id: &TransitionId,
        expected: &[TransitionPhase],
        next: TransitionPhase,
    ) -> Result<(), LifecycleError> {
        let transition = self.transition_mut(id)?;
        if transition.phase == next {
            return Ok(());
        }
        if !expected.contains(&transition.phase) {
            return UnexpectedPhaseSnafu {
                expected: expected
                    .first()
                    .copied()
                    .expect("advance_from requires an expected phase"),
                actual: transition.phase,
            }
            .fail();
        }
        transition.phase = next;
        Ok(())
    }
}

fn current_schema_version() -> u32 {
    LifecycleState::SCHEMA_VERSION
}

fn validate_transition_token(token: &str) -> Result<(), LifecycleError> {
    if token.trim().is_empty() {
        return EmptyTransitionTokenSnafu.fail();
    }
    Ok(())
}

#[derive(Debug, Snafu)]
pub enum LifecycleError {
    #[snafu(display("accelerator profile transition '{}' is already active", active_id))]
    ActiveTransitionExists { active_id: TransitionId },

    #[snafu(display("transition token must not be empty"))]
    EmptyTransitionToken,

    #[snafu(display("failure reason must not be empty"))]
    EmptyFailureReason,

    #[snafu(display("transition generation cannot advance beyond u64::MAX"))]
    GenerationExhausted,

    #[snafu(display("accelerator profile '{target_profile:?}' is already committed"))]
    ProfileAlreadyCommitted { target_profile: AcceleratorProfile },

    #[snafu(display("no accelerator profile transition is active"))]
    NoActiveTransition,

    #[snafu(display(
        "transition ID '{}' does not match active transition '{}'",
        actual,
        expected
    ))]
    TransitionIdMismatch {
        expected: TransitionId,
        actual: TransitionId,
    },

    #[snafu(display(
        "transition token '{}' was already used by generation {} with outcome '{outcome:?}'",
        token,
        generation
    ))]
    TransitionTokenAlreadyUsed {
        token: String,
        generation: u64,
        outcome: TransitionOutcome,
    },

    #[snafu(display(
        "stale lifecycle generation {}; current generation is {}",
        actual,
        expected
    ))]
    StaleGeneration { expected: u64, actual: u64 },

    #[snafu(display("expected transition phase '{expected:?}', found '{actual:?}'"))]
    UnexpectedPhase {
        expected: TransitionPhase,
        actual: TransitionPhase,
    },

    #[snafu(display("restore was already requested in phase '{phase:?}'"))]
    RestoreAlreadyRequested { phase: TransitionPhase },
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "transition-1";

    fn begin(state: &mut LifecycleState, target: AcceleratorProfile) -> TransitionId {
        let generation = state.generation();
        state.begin(TOKEN, target, generation).unwrap()
    }

    fn advance_to_qualification(state: &mut LifecycleState, id: &TransitionId) {
        state.record_node_drained(id).unwrap();
        state.record_advertisement_withdrawn(id).unwrap();
        state.record_target_profile_applied(id).unwrap();
        state.record_target_dra_validated(id).unwrap();
    }

    fn commit(state: &mut LifecycleState, token: &str, target: AcceleratorProfile) -> TransitionId {
        let generation = state.generation();
        let id = state.begin(token, target, generation).unwrap();
        advance_to_qualification(state, &id);
        state.record_target_qualified(&id).unwrap();
        state.commit_target(&id).unwrap();
        id
    }

    #[test]
    fn default_state_uses_current_schema_and_zero_generation() {
        let state = LifecycleState::default();
        assert_eq!(state.schema_version(), LifecycleState::SCHEMA_VERSION);
        assert_eq!(state.generation(), 0);
    }

    #[test]
    fn initial_profile_can_be_qualified_and_committed() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);

        assert_eq!(id.generation(), 1);
        assert_eq!(id.token(), TOKEN);
        assert_eq!(state.next_action(), Some(NextAction::CoordinatorDrainNode));
        advance_to_qualification(&mut state, &id);
        state.record_target_qualified(&id).unwrap();
        state.commit_target(&id).unwrap();

        assert_eq!(
            state.committed_profile(),
            Some(AcceleratorProfile::SharedInference)
        );
        assert_eq!(state.next_action(), None);
        let result = state.last_result().unwrap();
        assert_eq!(result.id(), &id);
        assert_eq!(result.previous_profile(), None);
        assert_eq!(result.target_profile(), AcceleratorProfile::SharedInference);
        assert_eq!(result.outcome(), TransitionOutcome::TargetCommitted);
        assert_eq!(result.failure_reason(), None);
    }

    #[test]
    fn failed_transition_restores_previous_profile() {
        let mut state = LifecycleState::new(Some(AcceleratorProfile::SharedInference));
        let id = begin(&mut state, AcceleratorProfile::DistributedTraining);
        advance_to_qualification(&mut state, &id);

        state
            .request_restore(&id, "NCCL all-reduce bandwidth below threshold")
            .unwrap();
        assert_eq!(
            state.next_action(),
            Some(NextAction::NodeApplyPreviousProfile)
        );
        state.record_previous_profile_applied(&id).unwrap();
        state.record_previous_dra_validated(&id).unwrap();
        state.commit_restore(&id).unwrap();

        assert_eq!(
            state.committed_profile(),
            Some(AcceleratorProfile::SharedInference)
        );
        let result = state.last_result().unwrap();
        assert_eq!(result.outcome(), TransitionOutcome::PreviousProfileRestored);
        assert_eq!(
            result.failure_reason(),
            Some("NCCL all-reduce bandwidth below threshold")
        );
    }

    #[test]
    fn failed_initial_provisioning_requires_manual_intervention() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::DistributedTraining);
        state.record_node_drained(&id).unwrap();
        state
            .request_restore(&id, "DRA ResourceSlice was not published")
            .unwrap();

        assert_eq!(state.phase(), Some(TransitionPhase::Blocked));
        assert_eq!(state.next_action(), Some(NextAction::ManualIntervention));
        assert_eq!(state.committed_profile(), None);
    }

    #[test]
    fn repeated_begin_and_acknowledgement_are_idempotent() {
        let mut state = LifecycleState::default();
        let id = state
            .begin(TOKEN, AcceleratorProfile::SharedInference, 0)
            .unwrap();
        let repeated = state
            .begin(TOKEN, AcceleratorProfile::SharedInference, 0)
            .unwrap();
        state.record_node_drained(&id).unwrap();
        state.record_node_drained(&id).unwrap();

        assert_eq!(repeated, id);
        assert_eq!(state.phase(), Some(TransitionPhase::NodeDrained));
    }

    #[test]
    fn repeated_commit_and_begin_after_lost_response_are_idempotent() {
        let mut state = LifecycleState::default();
        let id = commit(&mut state, TOKEN, AcceleratorProfile::SharedInference);

        state.commit_target(&id).unwrap();
        let repeated = state
            .begin(TOKEN, AcceleratorProfile::SharedInference, 0)
            .unwrap();

        assert_eq!(repeated, id);
        assert_eq!(
            state.committed_profile(),
            Some(AcceleratorProfile::SharedInference)
        );
    }

    #[test]
    fn old_begin_cannot_replay_after_a_newer_generation() {
        let mut state = LifecycleState::default();
        commit(
            &mut state,
            "transition-1",
            AcceleratorProfile::SharedInference,
        );
        commit(
            &mut state,
            "transition-2",
            AcceleratorProfile::DistributedTraining,
        );

        assert!(matches!(
            state
                .begin("transition-1", AcceleratorProfile::SharedInference, 0)
                .unwrap_err(),
            LifecycleError::StaleGeneration {
                expected: 2,
                actual: 0,
            }
        ));
        assert_eq!(
            state.committed_profile(),
            Some(AcceleratorProfile::DistributedTraining)
        );
    }

    #[test]
    fn stale_transition_event_is_rejected() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);
        let stale_id = TransitionId {
            generation: id.generation(),
            token: "stale-transition".to_string(),
        };

        assert!(matches!(
            state.record_node_drained(&stale_id).unwrap_err(),
            LifecycleError::TransitionIdMismatch { .. }
        ));
        assert_eq!(state.phase(), Some(TransitionPhase::IntentPersisted));
    }

    #[test]
    fn out_of_order_operation_is_rejected_without_losing_state() {
        let mut state = LifecycleState::new(Some(AcceleratorProfile::SharedInference));
        let id = begin(&mut state, AcceleratorProfile::DistributedTraining);

        let error = state.record_target_profile_applied(&id).unwrap_err();
        assert!(matches!(
            error,
            LifecycleError::UnexpectedPhase {
                expected: TransitionPhase::AdvertisementWithdrawn,
                actual: TransitionPhase::IntentPersisted,
            }
        ));
        assert_eq!(state.phase(), Some(TransitionPhase::IntentPersisted));
    }

    #[test]
    fn transition_to_committed_profile_is_rejected() {
        let mut state = LifecycleState::new(Some(AcceleratorProfile::SharedInference));
        let generation = state.generation();
        assert!(matches!(
            state
                .begin(TOKEN, AcceleratorProfile::SharedInference, generation)
                .unwrap_err(),
            LifecycleError::ProfileAlreadyCommitted {
                target_profile: AcceleratorProfile::SharedInference,
            }
        ));
    }

    #[test]
    fn next_action_identifies_component_ownership() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);
        assert_eq!(state.next_action(), Some(NextAction::CoordinatorDrainNode));
        state.record_node_drained(&id).unwrap();
        assert_eq!(
            state.next_action(),
            Some(NextAction::NodeWithdrawAdvertisement)
        );
        state.record_advertisement_withdrawn(&id).unwrap();
        assert_eq!(
            state.next_action(),
            Some(NextAction::NodeApplyTargetProfile)
        );
        state.record_target_profile_applied(&id).unwrap();
        assert_eq!(state.next_action(), Some(NextAction::NodeValidateTargetDra));
        state.record_target_dra_validated(&id).unwrap();
        assert_eq!(
            state.next_action(),
            Some(NextAction::CoordinatorRunQualification)
        );
    }

    #[test]
    fn target_reboot_requirement_is_durable_and_resumable() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);
        state.record_node_drained(&id).unwrap();
        state.record_advertisement_withdrawn(&id).unwrap();

        state.record_target_profile_reboot_required(&id).unwrap();
        assert_eq!(
            state.phase(),
            Some(TransitionPhase::TargetProfileRebootRequired)
        );
        assert_eq!(
            state.next_action(),
            Some(NextAction::NodeApplyTargetProfile)
        );

        state.record_target_profile_applied(&id).unwrap();
        assert_eq!(state.phase(), Some(TransitionPhase::TargetProfileApplied));
    }

    #[test]
    fn restore_reboot_requirement_is_durable_and_resumable() {
        let mut state = LifecycleState::new(Some(AcceleratorProfile::SharedInference));
        let id = begin(&mut state, AcceleratorProfile::DistributedTraining);
        state.record_node_drained(&id).unwrap();
        state.request_restore(&id, "qualification failed").unwrap();

        state.record_previous_profile_reboot_required(&id).unwrap();
        assert_eq!(
            state.next_action(),
            Some(NextAction::NodeApplyPreviousProfile)
        );

        state.record_previous_profile_applied(&id).unwrap();
        assert_eq!(state.phase(), Some(TransitionPhase::PreviousProfileApplied));
    }
}
