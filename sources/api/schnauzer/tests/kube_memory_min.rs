use schnauzer::v2::import::{JsonSettingsResolver, StaticHelperResolver};
use serde_json::{json, Value};

struct Importer {
    settings_resolver: JsonSettingsResolver,
    helper_resolver: StaticHelperResolver,
}
schnauzer::impl_template_importer!(Importer, JsonSettingsResolver, StaticHelperResolver);

async fn render(component: &str, quantities: Value) -> Result<String, Box<dyn std::error::Error>> {
    let importer = Importer {
        settings_resolver: JsonSettingsResolver::new(json!({
            "settings": {"kubernetes": {"kube-reserved-memory-min": quantities}}
        })),
        helper_resolver: StaticHelperResolver,
    };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../../packages/release/{component}-memory-min-conf"
    ));
    Ok(schnauzer::v2::render_template_file(&importer, path).await?)
}

#[tokio::test]
async fn templates_default_override_and_preserve_zero() {
    for (settings, expected) in [
        (Value::Null, [0_u64, 0, 0]),
        (json!({}), [0, 0, 0]),
        (
            json!({"kubelet": "128Mi", "containerd": "128Mi"}),
            [134217728, 134217728, 268435456],
        ),
        (
            json!({"kubelet": "200Mi", "containerd": "300Mi"}),
            [209715200, 314572800, 524288000],
        ),
        (json!({"containerd": "0"}), [0, 0, 0]),
        (json!({"kubelet": "0", "containerd": "0"}), [0, 0, 0]),
        (json!({"kubelet": "1", "containerd": "1"}), [1, 1, 2]),
        (
            json!({"kubelet": "9223372036854775807", "containerd": "9223372036854775807"}),
            [u64::MAX / 2, u64::MAX / 2, u64::MAX - 1],
        ),
        (
            json!({"kubelet": "1Gi", "containerd": "100Mi"}),
            [1073741824, 104857600, 1178599424],
        ),
    ] {
        for ((component, section), bytes) in [
            ("kubelet", "Service"),
            ("containerd", "Service"),
            ("runtime", "Slice"),
        ]
        .into_iter()
        .zip(expected)
        {
            let output = render(component, settings.clone()).await.unwrap();
            assert_eq!(output.trim(), format!("[{section}]\nMemoryMin={bytes}"));
        }
        assert_eq!(expected[2], expected[0] + expected[1]);
    }
}

#[tokio::test]
async fn templates_reject_unrepresentable_values_and_sum_overflow() {
    for quantities in [
        json!({"kubelet": "-1Mi"}),
        json!({"kubelet": "1..2Mi"}),
        json!({"kubelet": "1.5Gi"}),
        json!({"kubelet": "16Ei"}),
        json!({"kubelet": "1G"}),
        json!({"containerd": 123}),
        json!({"containerd": "1Mi\nMemoryMin=0"}),
        json!({"kubelet": "18446744073709551615", "containerd": "1"}),
        json!({"kubelet": "9223372036854775808", "containerd": "1"}),
        json!({"kubelet": "8589934592Gi"}),
    ] {
        assert!(render("runtime", quantities).await.is_err());
    }
}
