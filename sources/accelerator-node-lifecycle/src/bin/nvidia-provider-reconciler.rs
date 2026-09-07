use argh::FromArgs;
use std::fmt;
use std::io;
use std::process::{Command, ExitCode, Output};

const KUBELET_UNIT: &str = "kubelet.service";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const SYSTEMD_INACTIVE_EXIT_CODE: i32 = 3;

/// Restart one NVIDIA resource provider after Bottlerocket settings change.
#[derive(FromArgs)]
struct Args {
    /// NVIDIA provider systemd unit
    #[argh(positional, from_str_fn(parse_provider))]
    provider: NvidiaProvider,
}

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

fn parse_provider(value: &str) -> Result<NvidiaProvider, String> {
    match value {
        "nvidia-k8s-device-plugin.service" => Ok(NvidiaProvider::DevicePlugin),
        "nvidia-mps-control-daemon.service" => Ok(NvidiaProvider::MpsControlDaemon),
        "nvidia-dra-driver-gpu.service" => Ok(NvidiaProvider::DraDriver),
        _ => Err(format!("unsupported NVIDIA provider unit '{value}'")),
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

fn reconcile<M: ServiceManager>(
    provider: NvidiaProvider,
    service_manager: &mut M,
) -> io::Result<()> {
    // During first boot settings are rendered before kubelet starts.  Let
    // normal systemd ordering start the enabled provider at multi-user.target.
    if !service_manager.is_active(KUBELET_UNIT)? {
        return Ok(());
    }

    let exec_start = service_manager.exec_start(provider.unit())?;
    if provider.is_selected(&exec_start) {
        service_manager.restart(provider.unit())
    } else {
        // Every provider is affected by the atomic mode setting.  Stop an
        // unselected placeholder instead of starting it, which would stop the
        // selected peer through systemd's symmetric Conflicts= relationship.
        service_manager.stop(provider.unit())
    }
}

fn main() -> ExitCode {
    let args: Args = argh::from_env();
    match reconcile(args.provider, &mut Systemd) {
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

    #[derive(Default)]
    struct FakeServiceManager {
        kubelet_active: bool,
        active_queries: Vec<String>,
        restarts: Vec<String>,
        stops: Vec<String>,
        exec_start_queries: Vec<String>,
        exec_start: String,
    }

    impl ServiceManager for FakeServiceManager {
        fn is_active(&mut self, unit: &str) -> io::Result<bool> {
            self.active_queries.push(unit.to_string());
            Ok(self.kubelet_active)
        }

        fn exec_start(&mut self, unit: &str) -> io::Result<String> {
            self.exec_start_queries.push(unit.to_string());
            Ok(self.exec_start.clone())
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
    fn first_boot_does_not_start_a_provider_before_kubelet() {
        let mut manager = FakeServiceManager::default();

        reconcile(NvidiaProvider::DraDriver, &mut manager).unwrap();

        assert_eq!(manager.active_queries, [KUBELET_UNIT]);
        assert!(manager.restarts.is_empty());
        assert!(manager.stops.is_empty());
        assert!(manager.exec_start_queries.is_empty());
    }

    #[test]
    fn settings_change_restarts_a_selected_provider() {
        for provider in [
            NvidiaProvider::DevicePlugin,
            NvidiaProvider::MpsControlDaemon,
            NvidiaProvider::DraDriver,
        ] {
            let mut manager = FakeServiceManager {
                kubelet_active: true,
                exec_start: format!(
                    "{{ path={} ; argv[]={}; ignore_errors=no }}",
                    provider.executable(),
                    provider.executable()
                ),
                ..Default::default()
            };

            reconcile(provider, &mut manager).unwrap();

            assert_eq!(manager.active_queries, [KUBELET_UNIT]);
            assert_eq!(manager.exec_start_queries, [provider.unit()]);
            assert_eq!(manager.restarts, [provider.unit()]);
            assert!(manager.stops.is_empty());
        }
    }

    #[test]
    fn settings_change_stops_an_unselected_placeholder() {
        let mut manager = FakeServiceManager {
            kubelet_active: true,
            exec_start: "path=/usr/bin/true ; argv[]=/usr/bin/true".to_string(),
            ..Default::default()
        };
        for provider in [
            NvidiaProvider::DevicePlugin,
            NvidiaProvider::MpsControlDaemon,
            NvidiaProvider::DraDriver,
        ] {
            reconcile(provider, &mut manager).unwrap();
        }

        assert_eq!(manager.active_queries, [KUBELET_UNIT; 3]);
        assert_eq!(
            manager.exec_start_queries,
            [
                "nvidia-k8s-device-plugin.service",
                "nvidia-mps-control-daemon.service",
                "nvidia-dra-driver-gpu.service",
            ]
        );
        assert!(manager.restarts.is_empty());
        assert_eq!(
            manager.stops,
            [
                "nvidia-k8s-device-plugin.service",
                "nvidia-mps-control-daemon.service",
                "nvidia-dra-driver-gpu.service",
            ]
        );
    }

    #[test]
    fn supported_provider_unit_names_are_stable() {
        for (unit, provider) in [
            (
                "nvidia-k8s-device-plugin.service",
                NvidiaProvider::DevicePlugin,
            ),
            (
                "nvidia-mps-control-daemon.service",
                NvidiaProvider::MpsControlDaemon,
            ),
            ("nvidia-dra-driver-gpu.service", NvidiaProvider::DraDriver),
        ] {
            assert_eq!(parse_provider(unit).unwrap(), provider);
            assert_eq!(provider.unit(), unit);
            assert!(provider.is_selected(&format!("path={} ;", provider.executable())));
            assert!(
                !provider.is_selected(&format!("path={}-unexpected ;", provider.executable()))
            );
        }
    }

    #[test]
    fn unsupported_provider_is_rejected() {
        let error = parse_provider("containerd.service").unwrap_err();
        assert!(error.contains("unsupported NVIDIA provider unit"));
    }
}
