use crate::{AcceleratorProfile, LifecycleError, LifecycleState, NextAction};
use serde::Serialize;
use std::error::Error;
use std::fmt;

/// A node-local operation selected from durable lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "action", content = "profile")]
pub enum NodeAction {
    WithdrawAdvertisement,
    ApplyProfile(AcceleratorProfile),
    ValidateDra(AcceleratorProfile),
    CommitTarget,
    CommitRestore,
}

/// The result of one resume-engine decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "status", content = "detail")]
pub enum ResumeStatus {
    Idle,
    WaitingForCoordinator { action: NextAction },
    ManualIntervention,
    ActionCompleted { action: NodeAction },
}

/// Executes idempotent node operations without owning coordinator work.
pub trait NodeActionExecutor {
    type Error: Error;

    fn withdraw_advertisement(&mut self) -> Result<(), Self::Error>;
    fn apply_profile(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error>;
    fn validate_dra(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error>;
}

/// Executes at most one node-owned action and advances state only after it succeeds.
pub fn execute_next_node_action<E>(
    state: &mut LifecycleState,
    executor: &mut E,
) -> Result<ResumeStatus, ResumeError<E::Error>>
where
    E: NodeActionExecutor,
{
    let Some(next_action) = state.next_action() else {
        return Ok(ResumeStatus::Idle);
    };

    match next_action {
        NextAction::CoordinatorDrainNode | NextAction::CoordinatorRunQualification => {
            Ok(ResumeStatus::WaitingForCoordinator {
                action: next_action,
            })
        }
        NextAction::ManualIntervention => Ok(ResumeStatus::ManualIntervention),
        NextAction::NodeWithdrawAdvertisement => {
            let id = active_id(state)?;
            executor
                .withdraw_advertisement()
                .map_err(ResumeError::Executor)?;
            state
                .record_advertisement_withdrawn(&id)
                .map_err(ResumeError::Lifecycle)?;
            completed(NodeAction::WithdrawAdvertisement)
        }
        NextAction::NodeApplyTargetProfile => {
            let id = active_id(state)?;
            let profile = state
                .target_profile()
                .ok_or(ResumeError::InconsistentState("target profile is missing"))?;
            executor
                .apply_profile(profile)
                .map_err(ResumeError::Executor)?;
            state
                .record_target_profile_applied(&id)
                .map_err(ResumeError::Lifecycle)?;
            completed(NodeAction::ApplyProfile(profile))
        }
        NextAction::NodeValidateTargetDra => {
            let id = active_id(state)?;
            let profile = state
                .target_profile()
                .ok_or(ResumeError::InconsistentState("target profile is missing"))?;
            executor
                .validate_dra(profile)
                .map_err(ResumeError::Executor)?;
            state
                .record_target_dra_validated(&id)
                .map_err(ResumeError::Lifecycle)?;
            completed(NodeAction::ValidateDra(profile))
        }
        NextAction::NodeCommitTarget => {
            let id = active_id(state)?;
            state.commit_target(&id).map_err(ResumeError::Lifecycle)?;
            completed(NodeAction::CommitTarget)
        }
        NextAction::NodeApplyPreviousProfile => {
            let id = active_id(state)?;
            let profile = state
                .previous_profile()
                .ok_or(ResumeError::InconsistentState(
                    "previous profile is missing during restoration",
                ))?;
            executor
                .apply_profile(profile)
                .map_err(ResumeError::Executor)?;
            state
                .record_previous_profile_applied(&id)
                .map_err(ResumeError::Lifecycle)?;
            completed(NodeAction::ApplyProfile(profile))
        }
        NextAction::NodeValidatePreviousDra => {
            let id = active_id(state)?;
            let profile = state
                .previous_profile()
                .ok_or(ResumeError::InconsistentState(
                    "previous profile is missing during restoration",
                ))?;
            executor
                .validate_dra(profile)
                .map_err(ResumeError::Executor)?;
            state
                .record_previous_dra_validated(&id)
                .map_err(ResumeError::Lifecycle)?;
            completed(NodeAction::ValidateDra(profile))
        }
        NextAction::NodeCommitRestore => {
            let id = active_id(state)?;
            state.commit_restore(&id).map_err(ResumeError::Lifecycle)?;
            completed(NodeAction::CommitRestore)
        }
    }
}

fn active_id<E>(state: &LifecycleState) -> Result<crate::TransitionId, ResumeError<E>> {
    state
        .active_transition_id()
        .cloned()
        .ok_or(ResumeError::InconsistentState(
            "next action exists without an active transition",
        ))
}

fn completed<E>(action: NodeAction) -> Result<ResumeStatus, ResumeError<E>> {
    Ok(ResumeStatus::ActionCompleted { action })
}

#[derive(Debug)]
pub enum ResumeError<E> {
    Executor(E),
    Lifecycle(LifecycleError),
    InconsistentState(&'static str),
}

impl<E> fmt::Display for ResumeError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Executor(error) => write!(formatter, "node action failed: {error}"),
            Self::Lifecycle(error) => {
                write!(formatter, "failed to advance lifecycle state: {error}")
            }
            Self::InconsistentState(reason) => {
                write!(formatter, "inconsistent lifecycle state: {reason}")
            }
        }
    }
}

impl<E> Error for ResumeError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Executor(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
            Self::InconsistentState(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransitionId;
    use std::io;

    #[derive(Default)]
    struct FakeExecutor {
        actions: Vec<NodeAction>,
        failure: Option<&'static str>,
    }

    impl NodeActionExecutor for FakeExecutor {
        type Error = io::Error;

        fn withdraw_advertisement(&mut self) -> Result<(), Self::Error> {
            self.execute(NodeAction::WithdrawAdvertisement)
        }

        fn apply_profile(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error> {
            self.execute(NodeAction::ApplyProfile(profile))
        }

        fn validate_dra(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error> {
            self.execute(NodeAction::ValidateDra(profile))
        }
    }

    impl FakeExecutor {
        fn execute(&mut self, action: NodeAction) -> Result<(), io::Error> {
            if let Some(message) = self.failure {
                return Err(io::Error::other(message));
            }
            self.actions.push(action);
            Ok(())
        }
    }

    fn begin(state: &mut LifecycleState, target: AcceleratorProfile) -> TransitionId {
        state
            .begin("resume-test", target, state.generation())
            .unwrap()
    }

    #[test]
    fn coordinator_boundary_is_a_no_op() {
        let mut state = LifecycleState::default();
        begin(&mut state, AcceleratorProfile::SharedInference);
        let original = state.clone();

        let status = execute_next_node_action(&mut state, &mut FakeExecutor::default()).unwrap();

        assert_eq!(
            status,
            ResumeStatus::WaitingForCoordinator {
                action: NextAction::CoordinatorDrainNode
            }
        );
        assert_eq!(state, original);
    }

    #[test]
    fn withdraw_advances_only_after_executor_success() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);
        state.record_node_drained(&id).unwrap();
        let mut executor = FakeExecutor::default();

        let status = execute_next_node_action(&mut state, &mut executor).unwrap();

        assert_eq!(
            status,
            ResumeStatus::ActionCompleted {
                action: NodeAction::WithdrawAdvertisement
            }
        );
        assert_eq!(
            state.next_action(),
            Some(NextAction::NodeApplyTargetProfile)
        );
        assert_eq!(executor.actions, vec![NodeAction::WithdrawAdvertisement]);
    }

    #[test]
    fn target_profile_is_passed_to_apply_and_validate() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::DistributedTraining);
        state.record_node_drained(&id).unwrap();
        state.record_advertisement_withdrawn(&id).unwrap();
        let mut executor = FakeExecutor::default();

        execute_next_node_action(&mut state, &mut executor).unwrap();
        execute_next_node_action(&mut state, &mut executor).unwrap();

        assert_eq!(
            executor.actions,
            vec![
                NodeAction::ApplyProfile(AcceleratorProfile::DistributedTraining),
                NodeAction::ValidateDra(AcceleratorProfile::DistributedTraining),
            ]
        );
        assert_eq!(
            state.next_action(),
            Some(NextAction::CoordinatorRunQualification)
        );
    }

    #[test]
    fn executor_failure_leaves_state_unchanged() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);
        state.record_node_drained(&id).unwrap();
        let original = state.clone();
        let mut executor = FakeExecutor {
            failure: Some("injected failure"),
            ..Default::default()
        };

        assert!(matches!(
            execute_next_node_action(&mut state, &mut executor),
            Err(ResumeError::Executor(_))
        ));
        assert_eq!(state, original);
    }

    #[test]
    fn commit_target_needs_no_executor_operation() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);
        state.record_node_drained(&id).unwrap();
        state.record_advertisement_withdrawn(&id).unwrap();
        state.record_target_profile_applied(&id).unwrap();
        state.record_target_dra_validated(&id).unwrap();
        state.record_target_qualified(&id).unwrap();
        let mut executor = FakeExecutor::default();

        let status = execute_next_node_action(&mut state, &mut executor).unwrap();

        assert_eq!(
            status,
            ResumeStatus::ActionCompleted {
                action: NodeAction::CommitTarget
            }
        );
        assert!(executor.actions.is_empty());
        assert_eq!(
            state.committed_profile(),
            Some(AcceleratorProfile::SharedInference)
        );
    }

    #[test]
    fn restoration_uses_the_previous_profile_and_commits() {
        let mut state = LifecycleState::new(Some(AcceleratorProfile::SharedInference));
        let id = begin(&mut state, AcceleratorProfile::DistributedTraining);
        state.record_node_drained(&id).unwrap();
        state.request_restore(&id, "qualification failed").unwrap();
        let mut executor = FakeExecutor::default();

        execute_next_node_action(&mut state, &mut executor).unwrap();
        execute_next_node_action(&mut state, &mut executor).unwrap();
        let status = execute_next_node_action(&mut state, &mut executor).unwrap();

        assert_eq!(
            executor.actions,
            vec![
                NodeAction::ApplyProfile(AcceleratorProfile::SharedInference),
                NodeAction::ValidateDra(AcceleratorProfile::SharedInference),
            ]
        );
        assert_eq!(
            status,
            ResumeStatus::ActionCompleted {
                action: NodeAction::CommitRestore
            }
        );
        assert_eq!(
            state.committed_profile(),
            Some(AcceleratorProfile::SharedInference)
        );
    }

    #[test]
    fn manual_intervention_is_a_no_op() {
        let mut state = LifecycleState::default();
        let id = begin(&mut state, AcceleratorProfile::SharedInference);
        state.record_node_drained(&id).unwrap();
        state.request_restore(&id, "no previous profile").unwrap();
        let original = state.clone();

        let status = execute_next_node_action(&mut state, &mut FakeExecutor::default()).unwrap();

        assert_eq!(status, ResumeStatus::ManualIntervention);
        assert_eq!(state, original);
    }
}
