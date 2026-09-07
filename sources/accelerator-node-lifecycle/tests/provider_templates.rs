use schnauzer::v2::import::{JsonSettingsResolver, StaticHelperResolver};
use serde_json::{json, Value};

const DEVICE_PLUGIN_TEMPLATE: &str = include_str!(
    "../../../packages/nvidia-k8s-device-plugin/nvidia-k8s-device-plugin-exec-start-conf"
);
const DRA_TEMPLATE: &str =
    include_str!("../../../packages/nvidia-dra-driver-gpu/nvidia-dra-driver-gpu-exec-start-conf");
const MIG_TEMPLATE: &str =
    include_str!("../../../packages/nvidia-k8s-device-plugin/nvidia-k8s-device-plugin-mig-conf");
const MPS_TEMPLATE: &str = include_str!(
    "../../../packages/nvidia-k8s-device-plugin/nvidia-mps-control-daemon-exec-start-conf"
);
const DEVICE_PLUGIN_UNIT: &str =
    include_str!("../../../packages/nvidia-k8s-device-plugin/nvidia-k8s-device-plugin.service");
const MPS_UNIT: &str =
    include_str!("../../../packages/nvidia-k8s-device-plugin/nvidia-mps-control-daemon.service");
const DRA_UNIT: &str =
    include_str!("../../../packages/nvidia-dra-driver-gpu/nvidia-dra-driver-gpu.service");

struct TestImporter {
    settings_resolver: JsonSettingsResolver,
    helper_resolver: StaticHelperResolver,
}

impl TestImporter {
    fn new(settings: Value) -> Self {
        Self {
            settings_resolver: JsonSettingsResolver::new(settings),
            helper_resolver: StaticHelperResolver,
        }
    }
}

schnauzer::impl_template_importer!(TestImporter, JsonSettingsResolver, StaticHelperResolver);

async fn render(template: &str, settings: Value) -> String {
    schnauzer::v2::render_template_str(&TestImporter::new(settings), template)
        .await
        .unwrap()
}

fn explicit_mode(mode: &str) -> Value {
    json!({
        "settings": {
            "accelerators": {
                "nvidia": {
                    "mode": mode,
                    "mig": {
                        "profile": {
                            "a100.40gb": "2g.10gb",
                        },
                    },
                },
            },
            "kubernetes": {
                "hostname-override": "test-node",
            },
            "kubelet-device-plugins": {
                "nvidia": {
                    "enabled": true,
                    "device-sharing-strategy": "mps",
                    "device-partitioning-strategy": "mig",
                    "mig": {
                        "profile": {
                            "a100.40gb": "1g.5gb",
                        },
                    },
                },
            },
        },
    })
}

#[tokio::test]
async fn explicit_device_plugin_mode_selects_only_legacy_provider() {
    let settings = explicit_mode("device-plugin");
    let device_plugin = render(DEVICE_PLUGIN_TEMPLATE, settings.clone()).await;
    let dra = render(DRA_TEMPLATE, settings.clone()).await;
    let mig = render(MIG_TEMPLATE, settings.clone()).await;
    let mps = render(MPS_TEMPLATE, settings).await;

    assert!(device_plugin.contains("ExecStart=/usr/bin/nvidia-device-plugin"));
    assert!(device_plugin.contains("Conflicts=nvidia-dra-driver-gpu.service"));
    assert!(device_plugin.contains("After=nvidia-dra-driver-gpu.service"));
    assert!(!dra.contains("ExecStart=/usr/bin/gpu-kubelet-plugin"));
    assert!(!dra.contains("Conflicts=nvidia-k8s-device-plugin.service"));
    assert!(mig.contains("device-partitioning-strategy = \"mig\""));
    assert!(!mig.contains("strict-validation"));
    assert!(mps.contains("MPS and MIG are not supported at the same time"));
    assert!(mps.contains("Conflicts=nvidia-dra-driver-gpu.service"));
    assert!(mps.contains("After=nvidia-dra-driver-gpu.service"));
}

#[tokio::test]
async fn dra_profiles_select_only_dra_provider() {
    for (mode, partitioning_strategy) in [
        ("dra-shared-inference", "mig"),
        ("dra-distributed-training", "none"),
    ] {
        let settings = explicit_mode(mode);
        let device_plugin = render(DEVICE_PLUGIN_TEMPLATE, settings.clone()).await;
        let dra = render(DRA_TEMPLATE, settings.clone()).await;
        let mig = render(MIG_TEMPLATE, settings.clone()).await;
        let mps = render(MPS_TEMPLATE, settings).await;

        assert!(!device_plugin.contains("ExecStart=/usr/bin/nvidia-device-plugin"));
        assert!(!device_plugin.contains("Conflicts=nvidia-dra-driver-gpu.service"));
        assert!(dra.contains("ExecStart=/usr/bin/gpu-kubelet-plugin"));
        assert!(dra.contains(
            "Conflicts=nvidia-k8s-device-plugin.service nvidia-mps-control-daemon.service"
        ));
        assert!(dra.contains("Requires=nvidia-migmanager.service"));
        assert!(dra.contains(
            "After=nvidia-k8s-device-plugin.service nvidia-mps-control-daemon.service nvidia-migmanager.service"
        ));
        assert!(dra.contains("ConditionPathExists=!/run/nvidia-migmanager/reboot-required"));
        assert!(dra.contains("ExecStartPre=/usr/bin/nvidia-migmanager validate-mig"));
        assert!(mig.contains("strict-validation = true"));
        assert!(mig.contains(&format!(
            "device-partitioning-strategy = \"{partitioning_strategy}\""
        )));
        if mode == "dra-shared-inference" {
            assert!(mig.contains("2g.10gb"));
            assert!(!mig.contains("1g.5gb"));
        } else {
            assert!(!mig.contains("profile ="));
        }
        assert!(mps.trim().is_empty());
    }
}

#[tokio::test]
async fn disabled_mode_selects_no_provider() {
    let settings = explicit_mode("disabled");
    let device_plugin = render(DEVICE_PLUGIN_TEMPLATE, settings.clone()).await;
    let dra = render(DRA_TEMPLATE, settings).await;

    assert!(!device_plugin.contains("ExecStart=/usr/bin/nvidia-device-plugin"));
    assert!(!dra.contains("ExecStart=/usr/bin/gpu-kubelet-plugin"));
    assert!(!device_plugin.contains("Conflicts="));
    assert!(!dra.contains("Conflicts="));
}

#[tokio::test]
async fn absent_mode_preserves_legacy_enablement() {
    for (enabled, expect_device_plugin) in [(true, true), (false, false)] {
        let settings = json!({
            "settings": {
                "kubernetes": {
                    "hostname-override": "test-node",
                },
                "kubelet-device-plugins": {
                    "nvidia": {
                        "enabled": enabled,
                    },
                },
            },
        });
        let device_plugin = render(DEVICE_PLUGIN_TEMPLATE, settings.clone()).await;
        let dra = render(DRA_TEMPLATE, settings).await;

        assert_eq!(
            device_plugin.contains("ExecStart=/usr/bin/nvidia-device-plugin"),
            expect_device_plugin
        );
        assert_eq!(
            device_plugin.contains("Conflicts=nvidia-dra-driver-gpu.service"),
            expect_device_plugin
        );
        assert_eq!(
            device_plugin.contains("After=nvidia-dra-driver-gpu.service"),
            expect_device_plugin
        );
        assert!(!dra.contains("ExecStart=/usr/bin/gpu-kubelet-plugin"));
        assert!(!dra.contains("Conflicts="));
    }
}

#[tokio::test]
async fn explicit_mode_overrides_contradictory_legacy_setting() {
    let settings = explicit_mode("dra-shared-inference");
    let device_plugin = render(DEVICE_PLUGIN_TEMPLATE, settings.clone()).await;
    let dra = render(DRA_TEMPLATE, settings).await;

    assert!(!device_plugin.contains("ExecStart=/usr/bin/nvidia-device-plugin"));
    assert!(dra.contains("ExecStart=/usr/bin/gpu-kubelet-plugin"));
}

#[test]
fn base_units_do_not_conflict_with_inactive_provider_placeholders() {
    for unit in [DEVICE_PLUGIN_UNIT, MPS_UNIT, DRA_UNIT] {
        assert!(!unit.contains("Conflicts="));
        assert!(!unit.contains("After=nvidia-"));
    }
}
