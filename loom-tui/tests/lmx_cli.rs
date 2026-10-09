use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn gguf(path: &Path) {
    fn string(data: &mut Vec<u8>, value: &str) {
        data.extend((value.len() as u64).to_le_bytes());
        data.extend(value.as_bytes());
    }
    let mut data = b"GGUF".to_vec();
    data.extend(3u32.to_le_bytes());
    data.extend(1u64.to_le_bytes());
    data.extend(2u64.to_le_bytes());
    for (key, value) in [
        ("general.architecture", "qwen2"),
        ("tokenizer.chat_template", "{{ messages }}"),
    ] {
        string(&mut data, key);
        data.extend(8u32.to_le_bytes());
        string(&mut data, value);
    }
    string(&mut data, "dummy");
    data.extend(1u32.to_le_bytes());
    data.extend(2u64.to_le_bytes());
    data.extend(0u32.to_le_bytes());
    data.extend(0u64.to_le_bytes());
    while data.len() % 32 != 0 {
        data.push(0)
    }
    data.extend([0; 8]);
    fs::write(path, data).unwrap();
}

#[test]
fn lmx_import_list_inspect_and_explicit_engine_rejection_work_through_product_cli() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.gguf");
    gguf(&source);
    let root = work.path().join("models");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_loom"))
            .env("LOOM_MODELS", &root)
            .env("LOOM_MODEL_STATE_DIR", work.path().join("state"))
            // An explicit CLI engine must win over an invalid environment default.
            .env("LOOM_LMX_ENGINE", "invalid-default")
            .args(args)
            .output()
            .unwrap()
    };
    let output = run(&["model", "import", source.to_str().unwrap(), "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let prepared: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = prepared["id"].as_str().unwrap();
    assert!(id.starts_with("lmx/"));
    assert!(
        Path::new(prepared["path"].as_str().unwrap())
            .extension()
            .unwrap()
            == "lmx"
    );
    let listing = run(&["model", "list"]);
    assert!(listing.status.success());
    assert!(String::from_utf8_lossy(&listing.stdout).contains(id));
    let inspect = run(&["model", "inspect", id, "--verify", "--json"]);
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let inspected: Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(inspected["payload_verified"], true);
    assert!(
        inspected["engines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["engine"] == "cpu"
                && e["compiled"] == true
                && e["payload_supported"] == true)
    );
    let refused = run(&[
        "model", "bench", id, "--engine", "mlx", "--prompt", "8", "--gen", "1",
    ]);
    assert!(!refused.status.success());
    // The refusal names what the model needs that MLX lacks, not a generic "unsupported".
    assert!(String::from_utf8_lossy(&refused.stderr).contains("dummy has no MLX name"));
    assert_eq!(fs::read(&source).unwrap()[..4], *b"GGUF");
}
