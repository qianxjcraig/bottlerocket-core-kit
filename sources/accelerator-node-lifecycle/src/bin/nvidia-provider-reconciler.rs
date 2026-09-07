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

    service_manager.restart(provider.unit())
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
    }

    impl ServiceManager for FakeServiceManager {
        fn is_active(&mut self, unit: &str) -> io::Result<bool> {
            self.active_queries.push(unit.to_string());
            Ok(self.kubelet_active)
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
    }

    #[test]
    fn settings_change_restarts_only_the_requested_provider() {
        let mut manager = FakeServiceManager {
            kubelet_active: true,
            ..Default::default()
        };

        reconcile(NvidiaProvider::DevicePlugin, &mut manager).unwrap();

        assert_eq!(manager.active_queries, [KUBELET_UNIT]);
        assert_eq!(manager.restarts, ["nvidia-k8s-device-plugin.service"]);
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
        }
    }

    #[test]
    fn unsupported_provider_is_rejected() {
        let error = parse_provider("containerd.service").unwrap_err();
        assert!(error.contains("unsupported NVIDIA provider unit"));
    }
}
