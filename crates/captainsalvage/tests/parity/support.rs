use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;

pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn canonical(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let sorted: BTreeMap<_, _> = fields
                .into_iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "startedAtMillis"
                            | "updatedAtMillis"
                            | "resolvedAtMillis"
                            | "recordedAtMillis"
                            | "bytesPerSecond"
                            | "created"
                            | "modified"
                            | "accessed"
                    )
                })
                .map(|(key, value)| {
                    let value = if key == "path" {
                        match value {
                            Value::String(path) if path.starts_with('/') => {
                                Value::String(format!("volume:{path}"))
                            }
                            other => other,
                        }
                    } else {
                        value
                    };
                    (key, canonical(value))
                })
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        Value::String(text) => {
            assert!(
                !text.starts_with('/'),
                "unexpected absolute path in parity output: {text}"
            );
            Value::String(text)
        }
        other => other,
    }
}

pub fn record(name: &str, value: Value, reference: &str) {
    let actual = serde_json::to_string_pretty(&canonical(value)).expect("stable JSON") + "\n";
    if let Some(directory) = std::env::var_os("SALVAGE_PARITY_OUTPUT_DIR") {
        let path = std::path::Path::new(&directory).join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("new parity output, never overwrite a reference");
        file.write_all(actual.as_bytes()).expect("parity output");
    } else {
        assert!(
            actual.as_bytes() == reference.as_bytes(),
            "{name} differs from the pre-driver reference; run bash scripts/parity.sh for the diff"
        );
    }
}

#[test]
fn normalization_only_removes_documented_metadata_and_preserves_array_order() {
    let value = serde_json::json!({
        "z": [{"verdict": "partial", "modified": "fixture date", "id": 2, "path": "/visible"}, 1],
        "startedAtMillis": 123,
        "unknownFutureField": 7,
        "a": {"confidence": 0.75, "resolvedAtMillis": 456}
    });
    assert_eq!(
        serde_json::to_string(&canonical(value)).unwrap(),
        r#"{"a":{"confidence":0.75},"unknownFutureField":7,"z":[{"id":2,"path":"volume:/visible","verdict":"partial"},1]}"#
    );
}

#[test]
#[should_panic(expected = "unexpected absolute path in parity output")]
fn an_unexpected_host_path_is_not_silently_normalized() {
    canonical(serde_json::json!({"workspace": "/some/host/path"}));
}
