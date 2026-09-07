use accelerator_node_lifecycle::{
    AcceleratorProfile, LifecycleState, LockedJsonFileStore, NextAction, TransitionId,
};
use argh::FromArgs;
use serde::Serialize;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_STATE_PATH: &str = "/var/lib/accelerator-node-lifecycle/state.json";

/// Manage durable Bottlerocket accelerator profile transition state.
#[derive(FromArgs)]
struct Args {
    /// durable lifecycle state path
    #[argh(option, default = "PathBuf::from(DEFAULT_STATE_PATH)")]
    state_path: PathBuf,

    #[argh(subcommand)]
    command: Command,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Command {
    Status(StatusCommand),
    Begin(BeginCommand),
    Record(RecordCommand),
    Restore(RestoreCommand),
}

/// Print the current lifecycle state and next required action.
#[derive(FromArgs)]
#[argh(subcommand, name = "status")]
struct StatusCommand {}

/// Persist a new profile transition intent.
#[derive(FromArgs)]
#[argh(subcommand, name = "begin")]
struct BeginCommand {
    /// idempotency token supplied by the coordinator
    #[argh(option)]
    token: String,

    /// target profile: shared-inference or distributed-training
    #[argh(option, from_str_fn(parse_profile))]
    profile: AcceleratorProfile,

    /// state generation observed by the coordinator
    #[argh(option)]
    expected_generation: u64,
}

/// Record one successfully completed transition event.
#[derive(FromArgs)]
#[argh(subcommand, name = "record")]
struct RecordCommand {
    /// transition generation
    #[argh(option)]
    generation: u64,

    /// transition token
    #[argh(option)]
    token: String,

    /// completed event
    #[argh(option, from_str_fn(parse_event))]
    event: TransitionEvent,
}

/// Request restoration of the previous qualified profile.
#[derive(FromArgs)]
#[argh(subcommand, name = "restore")]
struct RestoreCommand {
    /// transition generation
    #[argh(option)]
    generation: u64,

    /// transition token
    #[argh(option)]
    token: String,

    /// non-empty failure reason retained in lifecycle state
    #[argh(option)]
    reason: String,
}

#[derive(Clone, Copy)]
enum TransitionEvent {
    NodeDrained,
    AdvertisementWithdrawn,
    TargetProfileApplied,
    TargetDraValidated,
    TargetQualified,
    TargetCommitted,
    PreviousProfileApplied,
    PreviousDraValidated,
    PreviousProfileCommitted,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct Status<'a> {
    state: &'a LifecycleState,
    next_action: Option<NextAction>,
}

fn main() -> ExitCode {
    match execute(argh::from_env()) {
        Ok(state) => {
            let status = Status {
                next_action: state.next_action(),
                state: &state,
            };
            if let Err(error) = serde_json::to_writer_pretty(std::io::stdout(), &status) {
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

fn execute(args: Args) -> Result<LifecycleState, Box<dyn Error>> {
    let store = LockedJsonFileStore::new(args.state_path);
    let guard = store.try_lock()?;
    let mut state = guard.load()?;

    match args.command {
        Command::Status(_) => {}
        Command::Begin(command) => {
            state.begin(command.token, command.profile, command.expected_generation)?;
            guard.save(&state)?;
        }
        Command::Record(command) => {
            let id = resolve_transition_id(&state, command.generation, &command.token)?;
            match command.event {
                TransitionEvent::NodeDrained => state.record_node_drained(&id)?,
                TransitionEvent::AdvertisementWithdrawn => {
                    state.record_advertisement_withdrawn(&id)?
                }
                TransitionEvent::TargetProfileApplied => {
                    state.record_target_profile_applied(&id)?
                }
                TransitionEvent::TargetDraValidated => state.record_target_dra_validated(&id)?,
                TransitionEvent::TargetQualified => state.record_target_qualified(&id)?,
                TransitionEvent::TargetCommitted => state.commit_target(&id)?,
                TransitionEvent::PreviousProfileApplied => {
                    state.record_previous_profile_applied(&id)?
                }
                TransitionEvent::PreviousDraValidated => {
                    state.record_previous_dra_validated(&id)?
                }
                TransitionEvent::PreviousProfileCommitted => state.commit_restore(&id)?,
            }
            guard.save(&state)?;
        }
        Command::Restore(command) => {
            let id = resolve_transition_id(&state, command.generation, &command.token)?;
            state.request_restore(&id, command.reason)?;
            guard.save(&state)?;
        }
    }

    Ok(state)
}

fn resolve_transition_id(
    state: &LifecycleState,
    generation: u64,
    token: &str,
) -> Result<TransitionId, io::Error> {
    let id = state
        .active_transition_id()
        .or_else(|| state.last_result().map(|result| result.id()))
        .ok_or_else(|| io::Error::other("no active or completed transition exists"))?;

    if id.generation() != generation || id.token() != token {
        return Err(io::Error::other(format!(
            "transition ID mismatch: requested {generation}:{token}, current {id}"
        )));
    }
    Ok(id.clone())
}

fn parse_profile(value: &str) -> Result<AcceleratorProfile, String> {
    match value {
        "shared-inference" => Ok(AcceleratorProfile::SharedInference),
        "distributed-training" => Ok(AcceleratorProfile::DistributedTraining),
        _ => Err(format!(
            "invalid profile '{value}'; expected shared-inference or distributed-training"
        )),
    }
}

fn parse_event(value: &str) -> Result<TransitionEvent, String> {
    match value {
        "node-drained" => Ok(TransitionEvent::NodeDrained),
        "advertisement-withdrawn" => Ok(TransitionEvent::AdvertisementWithdrawn),
        "target-profile-applied" => Ok(TransitionEvent::TargetProfileApplied),
        "target-dra-validated" => Ok(TransitionEvent::TargetDraValidated),
        "target-qualified" => Ok(TransitionEvent::TargetQualified),
        "target-committed" => Ok(TransitionEvent::TargetCommitted),
        "previous-profile-applied" => Ok(TransitionEvent::PreviousProfileApplied),
        "previous-dra-validated" => Ok(TransitionEvent::PreviousDraValidated),
        "previous-profile-committed" => Ok(TransitionEvent::PreviousProfileCommitted),
        _ => Err(format!("invalid transition event '{value}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn args(state_path: PathBuf, command: Command) -> Args {
        Args {
            state_path,
            command,
        }
    }

    #[test]
    fn command_flow_persists_and_commits_a_transition() {
        let directory = TempDir::new().unwrap();
        let state_path = directory.path().join("state.json");

        let state = execute(args(
            state_path.clone(),
            Command::Begin(BeginCommand {
                token: "coordinator-request".to_string(),
                profile: AcceleratorProfile::SharedInference,
                expected_generation: 0,
            }),
        ))
        .unwrap();
        let id = state.active_transition_id().unwrap().clone();

        for event in [
            TransitionEvent::NodeDrained,
            TransitionEvent::AdvertisementWithdrawn,
            TransitionEvent::TargetProfileApplied,
            TransitionEvent::TargetDraValidated,
            TransitionEvent::TargetQualified,
            TransitionEvent::TargetCommitted,
        ] {
            execute(args(
                state_path.clone(),
                Command::Record(RecordCommand {
                    generation: id.generation(),
                    token: id.token().to_string(),
                    event,
                }),
            ))
            .unwrap();
        }

        let state = execute(args(state_path, Command::Status(StatusCommand {}))).unwrap();
        assert_eq!(
            state.committed_profile(),
            Some(AcceleratorProfile::SharedInference)
        );
        assert!(state.active_transition_id().is_none());
        assert_eq!(state.next_action(), None);
    }

    #[test]
    fn stale_coordinator_id_does_not_modify_state() {
        let directory = TempDir::new().unwrap();
        let state_path = directory.path().join("state.json");
        execute(args(
            state_path.clone(),
            Command::Begin(BeginCommand {
                token: "current".to_string(),
                profile: AcceleratorProfile::DistributedTraining,
                expected_generation: 0,
            }),
        ))
        .unwrap();

        let result = execute(args(
            state_path.clone(),
            Command::Record(RecordCommand {
                generation: 1,
                token: "stale".to_string(),
                event: TransitionEvent::NodeDrained,
            }),
        ));

        assert!(result.is_err());
        let state = execute(args(state_path, Command::Status(StatusCommand {}))).unwrap();
        assert_eq!(state.next_action(), Some(NextAction::CoordinatorDrainNode));
    }
}
