use crate::{AcceleratorProfile, NodeActionExecutor};
use serde_json::Value;
use snafu::{ResultExt, Snafu};
#[cfg(test)]
use std::any::Any;
use std::io;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

const APICLIENT: &str = "/usr/bin/apiclient";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const KUBELET_UNIT: &str = "kubelet.service";
const DEVICE_PLUGIN_UNIT: &str = "nvidia-k8s-device-plugin.service";
const MPS_UNIT: &str = "nvidia-mps-control-daemon.service";
const DRA_UNIT: &str = "nvidia-dra-driver-gpu.service";
const DISABLED_MODE: &str = "disabled";
const SHARED_INFERENCE_MODE: &str = "dra-shared-inference";
const DISTRIBUTED_TRAINING_MODE: &str = "dra-distributed-training";
const SYSTEMD_INACTIVE_EXIT_CODE: i32 = 3;
const CONVERGENCE_ATTEMPTS: usize = 61;
const CONVERGENCE_INTERVAL: Duration = Duration::from_secs(1);

/// Applies and locally validates Bottlerocket's NVIDIA provider selection.
///
/// This executor validates the setting and systemd provider state only. Kubernetes ResourceSlice,
/// CDI, MIG geometry, and workload qualification belong to later lifecycle validation stages.
pub struct BottlerocketNodeActionExecutor {
    system: Box<dyn SystemInterface>,
}

impl BottlerocketNodeActionExecutor {
    pub fn new() -> Self {
        Self {
            system: Box::new(System),
        }
    }

    fn set_mode_and_wait(&mut self, expected_mode: &'static str) -> Result<(), NodeExecutorError> {
        if self.system.current_mode()?.as_deref() != Some(expected_mode) {
            self.system.set_mode(expected_mode)?;
        }
        self.wait_for_convergence(expected_mode)
    }

    fn validate_mode(&mut self, expected_mode: &'static str) -> Result<(), NodeExecutorError> {
        let actual = self.system.current_mode()?;
        if actual.as_deref() != Some(expected_mode) {
            return UnexpectedModeSnafu {
                expected: expected_mode,
                actual: actual.as_deref().unwrap_or("<unset>"),
            }
            .fail();
        }
        self.wait_for_convergence(expected_mode)
    }

    fn wait_for_convergence(
        &mut self,
        expected_mode: &'static str,
    ) -> Result<(), NodeExecutorError> {
        let mut last = self.system.provider_state()?;
        for attempt in 0..CONVERGENCE_ATTEMPTS {
            if last.matches_mode(expected_mode) {
                return Ok(());
            }
            if attempt + 1 < CONVERGENCE_ATTEMPTS {
                self.system.sleep(CONVERGENCE_INTERVAL);
                last = self.system.provider_state()?;
            }
        }

        ProviderConvergenceSnafu {
            expected: expected_mode,
            actual: format!("{last:?}"),
        }
        .fail()
    }
}

impl Default for BottlerocketNodeActionExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeActionExecutor for BottlerocketNodeActionExecutor {
    type Error = NodeExecutorError;

    fn withdraw_advertisement(&mut self) -> Result<(), Self::Error> {
        self.set_mode_and_wait(DISABLED_MODE)
    }

    fn apply_profile(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error> {
        self.set_mode_and_wait(mode_for_profile(profile))
    }

    fn validate_dra(&mut self, profile: AcceleratorProfile) -> Result<(), Self::Error> {
        self.validate_mode(mode_for_profile(profile))
    }
}

fn mode_for_profile(profile: AcceleratorProfile) -> &'static str {
    match profile {
        AcceleratorProfile::SharedInference => SHARED_INFERENCE_MODE,
        AcceleratorProfile::DistributedTraining => DISTRIBUTED_TRAINING_MODE,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProviderState {
    kubelet: bool,
    device_plugin: bool,
    mps: bool,
    dra: bool,
}

impl ProviderState {
    fn matches_mode(&self, mode: &str) -> bool {
        self.kubelet
            && !self.device_plugin
            && !self.mps
            && match mode {
                DISABLED_MODE => !self.dra,
                SHARED_INFERENCE_MODE | DISTRIBUTED_TRAINING_MODE => self.dra,
                _ => false,
            }
    }
}

trait SystemInterface {
    fn set_mode(&mut self, mode: &str) -> Result<(), NodeExecutorError>;
    fn current_mode(&mut self) -> Result<Option<String>, NodeExecutorError>;
    fn provider_state(&mut self) -> Result<ProviderState, NodeExecutorError>;
    fn sleep(&mut self, duration: Duration);
    #[cfg(test)]
    fn as_any(&self) -> &dyn Any;
}

struct System;

impl SystemInterface for System {
    fn set_mode(&mut self, mode: &str) -> Result<(), NodeExecutorError> {
        let setting = format!("settings.accelerators.nvidia.mode={mode}");
        let output = run(APICLIENT, &["set", &setting])?;
        command_succeeded(APICLIENT, &["set", &setting], &output)
    }

    fn current_mode(&mut self) -> Result<Option<String>, NodeExecutorError> {
        let arguments = ["get", "settings.accelerators.nvidia.mode"];
        let output = run(APICLIENT, &arguments)?;
        command_succeeded(APICLIENT, &arguments, &output)?;
        let settings: Value = serde_json::from_slice(&output.stdout).context(ParseSettingsSnafu)?;
        Ok(settings
            .pointer("/settings/accelerators/nvidia/mode")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    fn provider_state(&mut self) -> Result<ProviderState, NodeExecutorError> {
        Ok(ProviderState {
            kubelet: unit_is_active(KUBELET_UNIT)?,
            device_plugin: unit_is_active(DEVICE_PLUGIN_UNIT)?,
            mps: unit_is_active(MPS_UNIT)?,
            dra: unit_is_active(DRA_UNIT)?,
        })
    }

    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn run(program: &'static str, arguments: &[&str]) -> Result<Output, NodeExecutorError> {
    Command::new(program)
        .args(arguments)
        .output()
        .context(ExecuteCommandSnafu { program })
}

fn command_succeeded(
    program: &'static str,
    arguments: &[&str],
    output: &Output,
) -> Result<(), NodeExecutorError> {
    if output.status.success() {
        return Ok(());
    }

    CommandFailedSnafu {
        command: format!("{program} {}", arguments.join(" ")),
        status: output.status.to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim(),
    }
    .fail()
}

fn unit_is_active(unit: &'static str) -> Result<bool, NodeExecutorError> {
    let arguments = ["--quiet", "is-active", unit];
    let output = run(SYSTEMCTL, &arguments)?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(SYSTEMD_INACTIVE_EXIT_CODE) {
        return Ok(false);
    }

    command_succeeded(SYSTEMCTL, &arguments, &output)?;
    unreachable!("non-successful systemctl result returned success")
}

#[derive(Debug, Snafu)]
pub enum NodeExecutorError {
    #[snafu(display("failed to execute '{}': {}", program, source))]
    ExecuteCommand {
        program: &'static str,
        source: io::Error,
    },

    #[snafu(display("'{}' exited with {}: {}", command, status, stderr))]
    CommandFailed {
        command: String,
        status: String,
        stderr: String,
    },

    #[snafu(display("failed to parse Bottlerocket settings: {}", source))]
    ParseSettings { source: serde_json::Error },

    #[snafu(display("NVIDIA accelerator mode is '{}', expected '{}'", actual, expected))]
    UnexpectedMode {
        expected: &'static str,
        actual: String,
    },

    #[snafu(display(
        "NVIDIA providers did not converge to mode '{}'; last observed state: {}",
        expected,
        actual
    ))]
    ProviderConvergence {
        expected: &'static str,
        actual: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeSystem {
        mode: Option<String>,
        states: VecDeque<ProviderState>,
        set_modes: Vec<String>,
        sleeps: usize,
    }

    impl FakeSystem {
        fn new(mode: Option<&str>, states: impl IntoIterator<Item = ProviderState>) -> Self {
            Self {
                mode: mode.map(str::to_string),
                states: states.into_iter().collect(),
                set_modes: Vec::new(),
                sleeps: 0,
            }
        }
    }

    impl SystemInterface for FakeSystem {
        fn set_mode(&mut self, mode: &str) -> Result<(), NodeExecutorError> {
            self.mode = Some(mode.to_string());
            self.set_modes.push(mode.to_string());
            Ok(())
        }

        fn current_mode(&mut self) -> Result<Option<String>, NodeExecutorError> {
            Ok(self.mode.clone())
        }

        fn provider_state(&mut self) -> Result<ProviderState, NodeExecutorError> {
            Ok(if self.states.len() > 1 {
                self.states.pop_front().unwrap()
            } else {
                self.states.front().cloned().unwrap_or_default()
            })
        }

        fn sleep(&mut self, _duration: Duration) {
            self.sleeps += 1;
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn executor(system: FakeSystem) -> BottlerocketNodeActionExecutor {
        BottlerocketNodeActionExecutor {
            system: Box::new(system),
        }
    }

    fn dra_state() -> ProviderState {
        ProviderState {
            kubelet: true,
            dra: true,
            ..Default::default()
        }
    }

    fn disabled_state() -> ProviderState {
        ProviderState {
            kubelet: true,
            ..Default::default()
        }
    }

    #[test]
    fn withdraw_sets_disabled_and_waits_for_legacy_providers_to_stop() {
        let legacy = ProviderState {
            kubelet: true,
            device_plugin: true,
            mps: true,
            ..Default::default()
        };
        let mut executor = executor(FakeSystem::new(
            Some("device-plugin"),
            [legacy, disabled_state()],
        ));

        executor.withdraw_advertisement().unwrap();

        assert_eq!(
            executor.system.current_mode().unwrap().as_deref(),
            Some(DISABLED_MODE)
        );
    }

    #[test]
    fn apply_profile_uses_distinct_dra_modes() {
        for (profile, mode) in [
            (AcceleratorProfile::SharedInference, SHARED_INFERENCE_MODE),
            (
                AcceleratorProfile::DistributedTraining,
                DISTRIBUTED_TRAINING_MODE,
            ),
        ] {
            let mut executor = executor(FakeSystem::new(Some(DISABLED_MODE), [dra_state()]));
            executor.apply_profile(profile).unwrap();
            assert_eq!(
                executor.system.current_mode().unwrap().as_deref(),
                Some(mode)
            );
        }
    }

    #[test]
    fn repeated_apply_does_not_rewrite_an_already_selected_mode() {
        let system = FakeSystem::new(Some(SHARED_INFERENCE_MODE), [dra_state()]);
        let mut executor = executor(system);

        executor
            .apply_profile(AcceleratorProfile::SharedInference)
            .unwrap();

        let fake = executor
            .system
            .as_any()
            .downcast_ref::<FakeSystem>()
            .expect("fake system");
        assert!(fake.set_modes.is_empty());
    }

    #[test]
    fn validation_rejects_the_wrong_setting_even_if_services_match() {
        let mut executor = executor(FakeSystem::new(
            Some(DISTRIBUTED_TRAINING_MODE),
            [dra_state()],
        ));

        assert!(matches!(
            executor
                .validate_dra(AcceleratorProfile::SharedInference)
                .unwrap_err(),
            NodeExecutorError::UnexpectedMode { .. }
        ));
    }

    #[test]
    fn convergence_rejects_mutually_exclusive_providers_running_together() {
        let conflicting = ProviderState {
            kubelet: true,
            device_plugin: true,
            dra: true,
            ..Default::default()
        };
        let mut executor = executor(FakeSystem::new(
            Some(SHARED_INFERENCE_MODE),
            std::iter::repeat_n(conflicting, CONVERGENCE_ATTEMPTS),
        ));

        assert!(matches!(
            executor
                .validate_dra(AcceleratorProfile::SharedInference)
                .unwrap_err(),
            NodeExecutorError::ProviderConvergence { .. }
        ));
    }
}
