use accelerator_node_lifecycle::{
    execute_next_node_action, BottlerocketNodeActionExecutor, LifecycleState, LockedJsonFileStore,
    NodeAction, ResumeStatus,
};
use argh::FromArgs;
use serde::Serialize;
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_STATE_PATH: &str = "/var/lib/accelerator-node-lifecycle/state.json";
const MAX_ACTIONS_PER_RUN: usize = 8;

/// Resume durable node-owned accelerator lifecycle actions.
#[derive(FromArgs)]
struct Args {
    /// durable lifecycle state path
    #[argh(option, default = "PathBuf::from(DEFAULT_STATE_PATH)")]
    state_path: PathBuf,
}

fn main() -> ExitCode {
    match execute(argh::from_env(), &mut BottlerocketNodeActionExecutor::new()) {
        Ok(report) => {
            if let Err(error) = serde_json::to_writer_pretty(std::io::stdout(), &report) {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
            println!();
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn execute<E>(args: Args, executor: &mut E) -> Result<OwnedReport, Box<dyn Error>>
where
    E: accelerator_node_lifecycle::NodeActionExecutor,
    E::Error: 'static,
{
    let store = LockedJsonFileStore::new(args.state_path);
    let guard = store.try_lock()?;
    let mut state = guard.load()?;
    let mut completed_actions = Vec::new();

    let status = loop {
        let status = execute_next_node_action(&mut state, executor)?;
        match status {
            ResumeStatus::ActionCompleted { action } => {
                guard.save(&state)?;
                completed_actions.push(action);
                if completed_actions.len() >= MAX_ACTIONS_PER_RUN {
                    return Err(format!(
                        "resume exceeded the safety limit of {MAX_ACTIONS_PER_RUN} node actions"
                    )
                    .into());
                }
            }
            terminal => break terminal,
        }
    };

    Ok(OwnedReport {
        completed_actions,
        status,
        state,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct OwnedReport {
    completed_actions: Vec<NodeAction>,
    status: ResumeStatus,
    state: LifecycleState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use accelerator_node_lifecycle::{
        AcceleratorProfile, JsonFileStore, NextAction, NodeActionExecutor,
    };
    use std::io;
    use tempfile::TempDir;

    #[derive(Default)]
    struct FakeExecutor {
        actions: Vec<NodeAction>,
        fail_on: Option<NodeAction>,
    }

    impl FakeExecutor {
        fn run(&mut self, action: NodeAction) -> Result<(), io::Error> {
            if self.fail_on == Some(action) {
                return Err(io::Error::other("injected node action failure"));
            }
            self.actions.push(action);
            Ok(())
        }
    }

    impl NodeActionExecutor for FakeExecutor {
        type Error = io::Error;

        fn withdraw_advertisement(&mut self) -> Result<(), Self::Error> {
            self.run(NodeAction::WithdrawAdvertisement)
        }

        fn apply_profile(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error> {
            self.run(NodeAction::ApplyProfile(profile))
        }

        fn validate_dra(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error> {
            self.run(NodeAction::ValidateDra(profile))
        }
    }

    fn node_drained_state(path: &std::path::Path) {
        let mut state = LifecycleState::default();
        let id = state
            .begin("resume-binary-test", AcceleratorProfile::SharedInference, 0)
            .unwrap();
        state.record_node_drained(&id).unwrap();
        JsonFileStore::new(path).save(&state).unwrap();
    }

    #[test]
    fn run_checkpoints_each_action_and_stops_at_coordinator_boundary() {
        let directory = TempDir::new().unwrap();
        let state_path = directory.path().join("state.json");
        node_drained_state(&state_path);
        let mut executor = FakeExecutor::default();

        let report = execute(
            Args {
                state_path: state_path.clone(),
            },
            &mut executor,
        )
        .unwrap();

        assert_eq!(
            report.completed_actions,
            vec![
                NodeAction::WithdrawAdvertisement,
                NodeAction::ApplyProfile(AcceleratorProfile::SharedInference),
                NodeAction::ValidateDra(AcceleratorProfile::SharedInference),
            ]
        );
        assert_eq!(
            report.status,
            ResumeStatus::WaitingForCoordinator {
                action: NextAction::CoordinatorRunQualification
            }
        );
        let persisted = JsonFileStore::new(state_path).load().unwrap();
        assert_eq!(
            persisted.next_action(),
            Some(NextAction::CoordinatorRunQualification)
        );
    }

    #[test]
    fn failed_action_keeps_the_last_successful_checkpoint() {
        let directory = TempDir::new().unwrap();
        let state_path = directory.path().join("state.json");
        node_drained_state(&state_path);
        let mut executor = FakeExecutor {
            fail_on: Some(NodeAction::ValidateDra(AcceleratorProfile::SharedInference)),
            ..Default::default()
        };

        assert!(execute(
            Args {
                state_path: state_path.clone()
            },
            &mut executor
        )
        .is_err());

        let persisted = JsonFileStore::new(state_path).load().unwrap();
        assert_eq!(
            persisted.next_action(),
            Some(NextAction::NodeValidateTargetDra)
        );
    }
}
