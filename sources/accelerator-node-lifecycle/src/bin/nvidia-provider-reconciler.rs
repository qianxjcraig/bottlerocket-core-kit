use argh::FromArgs;
use std::fmt;
use std::io;
use std::process::{Command, ExitCode, Output};

const KUBELET_UNIT: &str = "kubelet.service";
const BOOT_COMPLETION_UNIT: &str = "multi-user.target";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const SYSTEMD_INACTIVE_EXIT_CODE: i32 = 3;
const PROVIDERS: [NvidiaProvider; 3] = [
    NvidiaProvider::DevicePlugin,
    NvidiaProvider::MpsControlDaemon,
    NvidiaProvider::DraDriver,
];
const START_ORDER: [NvidiaProvider; 3] = [
    NvidiaProvider::MpsControlDaemon,
    NvidiaProvider::DevicePlugin,
    NvidiaProvider::DraDriver,
];

/// Reconcile NVIDIA resource providers after a Bottlerocket settings change.
#[derive(FromArgs)]
struct Args {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NvidiaProvider {
    DevicePlugin,
    MpsControlDaemon,
    DraDriver,
}

impl NvidiaProvider {
    fn unit(self) -> &'static str {
        match self {
            Self::DevicePlugin => "nvidia-k8s-device-plugin.service",
            Self::MpsControlDaemon => "nvidia-mps-control-daemon.service",
            Self::DraDriver => "nvidia-dra-driver-gpu.service",
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::DevicePlugin => "/usr/bin/nvidia-device-plugin",
            Self::MpsControlDaemon => "/usr/bin/mps-control-daemon",
            Self::DraDriver => "/usr/bin/gpu-kubelet-plugin",
        }
    }

    fn is_selected(self, exec_start: &str) -> bool {
        let expected_path = format!("path={} ;", self.executable());
        exec_start.contains(&expected_path)
    }
}

impl fmt::Display for NvidiaProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.unit())
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ProviderSelection {
    device_plugin: bool,
    mps_control_daemon: bool,
    dra_driver: bool,
}

impl ProviderSelection {
    fn includes(&self, provider: NvidiaProvider) -> bool {
        match provider {
            NvidiaProvider::DevicePlugin => self.device_plugin,
            NvidiaProvider::MpsControlDaemon => self.mps_control_daemon,
            NvidiaProvider::DraDriver => self.dra_driver,
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.dra_driver && (self.device_plugin || self.mps_control_daemon) {
            return Err(io::Error::other(
                "rendered configuration selects DRA and legacy NVIDIA providers together",
            ));
        }
        if self.mps_control_daemon && !self.device_plugin {
            return Err(io::Error::other(
                "rendered configuration selects NVIDIA MPS without the device plugin",
            ));
        }
        Ok(())
    }
}

trait ServiceManager {
    fn is_active(&mut self, unit: &str) -> io::Result<bool>;
    fn exec_start(&mut self, unit: &str) -> io::Result<String>;
    fn stop(&mut self, unit: &str) -> io::Result<()>;
    fn restart(&mut self, unit: &str) -> io::Result<()>;
}

struct Systemd;

impl ServiceManager for Systemd {
    fn is_active(&mut self, unit: &str) -> io::Result<bool> {
        let output = Command::new(SYSTEMCTL)
            .args(["--quiet", "is-active", unit])
            .output()?;

        if output.status.success() {
            return Ok(true);
        }
        if output.status.code() == Some(SYSTEMD_INACTIVE_EXIT_CODE) {
            return Ok(false);
        }

        Err(command_failure("query active state for", unit, &output))
    }

    fn exec_start(&mut self, unit: &str) -> io::Result<String> {
        let output = Command::new(SYSTEMCTL)
            .args(["show", "--property=ExecStart", "--value", unit])
            .output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(command_failure("query ExecStart for", unit, &output))
        }
    }

    fn stop(&mut self, unit: &str) -> io::Result<()> {
        let output = Command::new(SYSTEMCTL).args(["stop", unit]).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure("stop", unit, &output))
        }
    }

    fn restart(&mut self, unit: &str) -> io::Result<()> {
        let output = Command::new(SYSTEMCTL).args(["restart", unit]).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure("restart", unit, &output))
        }
    }
}

fn command_failure(action: &str, unit: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    io::Error::other(format!(
        "failed to {action} {unit}: systemctl exited with {}: {}",
        output.status,
        stderr.trim()
    ))
}

fn rendered_selection<M: ServiceManager>(
    service_manager: &mut M,
) -> io::Result<ProviderSelection> {
    let mut selection = ProviderSelection::default();
    for provider in PROVIDERS {
        let selected = provider.is_selected(&service_manager.exec_start(provider.unit())?);
        match provider {
            NvidiaProvider::DevicePlugin => selection.device_plugin = selected,
            NvidiaProvider::MpsControlDaemon => selection.mps_control_daemon = selected,
            NvidiaProvider::DraDriver => selection.dra_driver = selected,
        }
    }
    selection.validate()?;
    Ok(selection)
}

fn reconcile<M: ServiceManager>(service_manager: &mut M) -> io::Result<()> {
    // During first boot settings are rendered before kubelet and the boot
    // target start. Let normal systemd ordering start the enabled provider.
    // Once boot is complete, an inactive kubelet is an outage rather than a
    // startup condition. Fail without touching providers so the settings
    // change can be retried after kubelet recovers.
    if !service_manager.is_active(KUBELET_UNIT)? {
        if !service_manager.is_active(BOOT_COMPLETION_UNIT)? {
            return Ok(());
        }
        return Err(io::Error::other(
            "refusing to reconcile NVIDIA providers while kubelet is inactive after boot",
        ));
    }

    let selection = rendered_selection(service_manager)?;

    // Stop every provider first so no order returned by settings-applier can
    // leave mutually exclusive providers active.  MPS starts before the device
    // plugin because the legacy unit requires it when MPS sharing is enabled.
    for provider in PROVIDERS {
        service_manager.stop(provider.unit())?;
    }
    for provider in START_ORDER {
        if selection.includes(provider) {
            service_manager.restart(provider.unit())?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let _: Args = argh::from_env();
    match reconcile(&mut Systemd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeServiceManager {
        active_units: HashMap<String, bool>,
        active_queries: Vec<String>,
        restarts: Vec<String>,
        stops: Vec<String>,
        exec_start_queries: Vec<String>,
        exec_starts: HashMap<String, String>,
    }

    impl FakeServiceManager {
        fn with_selection(selected: &[NvidiaProvider]) -> Self {
            let exec_starts = PROVIDERS
                .into_iter()
                .map(|provider| {
                    let executable = if selected.contains(&provider) {
                        provider.executable()
                    } else {
                        "/usr/bin/true"
                    };
                    (
                        provider.unit().to_string(),
                        format!("{{ path={executable} ; argv[]={executable} }}"),
                    )
                })
                .collect();
            Self {
                active_units: HashMap::from([
                    (KUBELET_UNIT.to_string(), true),
                    (BOOT_COMPLETION_UNIT.to_string(), true),
                ]),
                exec_starts,
                ..Default::default()
            }
        }
    }

    impl ServiceManager for FakeServiceManager {
        fn is_active(&mut self, unit: &str) -> io::Result<bool> {
            self.active_queries.push(unit.to_string());
            Ok(self.active_units.get(unit).copied().unwrap_or(false))
        }

        fn exec_start(&mut self, unit: &str) -> io::Result<String> {
            self.exec_start_queries.push(unit.to_string());
            Ok(self.exec_starts.get(unit).cloned().unwrap_or_default())
        }

        fn stop(&mut self, unit: &str) -> io::Result<()> {
            self.stops.push(unit.to_string());
            Ok(())
        }

        fn restart(&mut self, unit: &str) -> io::Result<()> {
            self.restarts.push(unit.to_string());
            Ok(())
        }
    }

    #[test]
    fn first_boot_leaves_provider_startup_to_systemd() {
        let mut manager = FakeServiceManager::default();

        reconcile(&mut manager).unwrap();

        assert_eq!(
            manager.active_queries,
            [KUBELET_UNIT, BOOT_COMPLETION_UNIT]
        );
        assert!(manager.exec_start_queries.is_empty());
        assert!(manager.stops.is_empty());
        assert!(manager.restarts.is_empty());
    }

    #[test]
    fn runtime_kubelet_outage_fails_without_changing_providers() {
        let mut manager = FakeServiceManager::default();
        manager
            .active_units
            .insert(BOOT_COMPLETION_UNIT.to_string(), true);

        assert!(reconcile(&mut manager).is_err());
        assert_eq!(
            manager.active_queries,
            [KUBELET_UNIT, BOOT_COMPLETION_UNIT]
        );
        assert!(manager.exec_start_queries.is_empty());
        assert!(manager.stops.is_empty());
        assert!(manager.restarts.is_empty());
    }

    #[test]
    fn disabled_mode_stops_all_providers() {
        let mut manager = FakeServiceManager::with_selection(&[]);

        reconcile(&mut manager).unwrap();

        assert_eq!(manager.exec_start_queries, provider_units());
        assert_eq!(manager.stops, provider_units());
        assert!(manager.restarts.is_empty());
    }

    #[test]
    fn device_plugin_mode_starts_only_the_device_plugin() {
        let mut manager =
            FakeServiceManager::with_selection(&[NvidiaProvider::DevicePlugin]);

        reconcile(&mut manager).unwrap();

        assert_eq!(manager.stops, provider_units());
        assert_eq!(manager.restarts, ["nvidia-k8s-device-plugin.service"]);
    }

    #[test]
    fn mps_mode_starts_mps_before_the_device_plugin() {
        let mut manager = FakeServiceManager::with_selection(&[
            NvidiaProvider::DevicePlugin,
            NvidiaProvider::MpsControlDaemon,
        ]);

        reconcile(&mut manager).unwrap();

        assert_eq!(manager.stops, provider_units());
        assert_eq!(
            manager.restarts,
            [
                "nvidia-mps-control-daemon.service",
                "nvidia-k8s-device-plugin.service",
            ]
        );
    }

    #[test]
    fn dra_mode_starts_only_the_dra_driver() {
        let mut manager = FakeServiceManager::with_selection(&[NvidiaProvider::DraDriver]);

        reconcile(&mut manager).unwrap();

        assert_eq!(manager.stops, provider_units());
        assert_eq!(manager.restarts, ["nvidia-dra-driver-gpu.service"]);
    }

    #[test]
    fn contradictory_provider_selection_is_rejected_before_changes() {
        for selected in [
            vec![
                NvidiaProvider::DevicePlugin,
                NvidiaProvider::DraDriver,
            ],
            vec![NvidiaProvider::MpsControlDaemon],
        ] {
            let mut manager = FakeServiceManager::with_selection(&selected);

            assert!(reconcile(&mut manager).is_err());
            assert!(manager.stops.is_empty());
            assert!(manager.restarts.is_empty());
        }
    }

    #[test]
    fn provider_unit_and_executable_names_are_stable() {
        for provider in PROVIDERS {
            assert!(provider.unit().starts_with("nvidia-"));
            assert!(provider.is_selected(&format!("path={} ;", provider.executable())));
            assert!(
                !provider.is_selected(&format!("path={}-unexpected ;", provider.executable()))
            );
        }
    }

    fn provider_units() -> Vec<String> {
        PROVIDERS
            .into_iter()
            .map(|provider| provider.unit().to_string())
            .collect()
    }
}
