use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Seek, SeekFrom, Write},
    path::Path,
    process::{Command, Output},
};

// A complete, tiny MLX package: no test-only Core helpers or external models.
fn mlx_fixture(directory: &Path, moe: bool) {
    fs::create_dir_all(directory).unwrap();
    let mut config = json!({
        "model_type": if moe { "qwen3_moe" } else { "qwen3" },
        "hidden_size":4, "intermediate_size":4, "num_hidden_layers":1,
        "num_attention_heads":1, "num_key_value_heads":1, "head_dim":4,
        "vocab_size":4, "max_position_embeddings":32, "tie_word_embeddings":true,
        "rope_theta":10000.0, "rms_norm_eps":0.000001,
        "eos_token_id":1
    });
    if moe {
        config["moe_intermediate_size"] = json!(4);
        config["num_experts"] = json!(2);
        config["num_experts_per_tok"] = json!(1);
        config["norm_topk_prob"] = json!(false);
        config["decoder_sparse_step"] = json!(1);
        config["mlp_only_layers"] = json!([]);
    }
    fs::write(directory.join("config.json"), config.to_string()).unwrap();
    fs::write(
        directory.join("tokenizer.json"),
        json!({
            "version":"1.0", "truncation":null, "padding":null, "added_tokens":[],
            "normalizer":null, "pre_tokenizer":{"type":"Whitespace"},
            "post_processor":null, "decoder":null,
            "model":{"type":"BPE", "dropout":null, "unk_token":"[UNK]",
                "continuing_subword_prefix":null, "end_of_word_suffix":null,
                "fuse_unk":false, "byte_fallback":false,
                "vocab":{"[UNK]":0,"[EOS]":1,"a":2,"b":3}, "merges":[]}
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        directory.join("tokenizer_config.json"),
        json!({
            "chat_template":"{% for m in messages %}{{ m.content }}{% endfor %}"
        })
        .to_string(),
    )
    .unwrap();

    let mut shapes = BTreeMap::new();
    shapes.insert("model.embed_tokens.weight".to_owned(), vec![4, 4]);
    shapes.insert("model.norm.weight".to_owned(), vec![4]);
    for name in ["input_layernorm", "post_attention_layernorm"] {
        shapes.insert(format!("model.layers.0.{name}.weight"), vec![4]);
    }
    for name in ["q_norm", "k_norm"] {
        shapes.insert(format!("model.layers.0.self_attn.{name}.weight"), vec![4]);
    }
    for name in ["q_proj", "k_proj", "v_proj", "o_proj"] {
        shapes.insert(
            format!("model.layers.0.self_attn.{name}.weight"),
            vec![4, 4],
        );
    }
    if moe {
        shapes.insert("model.layers.0.mlp.gate.weight".into(), vec![2, 4]);
    }
    for name in ["gate_proj", "up_proj", "down_proj"] {
        let prefix = if moe { "switch_mlp." } else { "" };
        shapes.insert(
            format!("model.layers.0.mlp.{prefix}{name}.weight"),
            if moe { vec![2, 4, 4] } else { vec![4, 4] },
        );
    }
    let mut header = BTreeMap::new();
    let mut offset = 0u64;
    for (name, shape) in shapes {
        let bytes = shape.iter().product::<u64>() * 2;
        header.insert(
            name,
            json!({"dtype":"F16", "shape":shape,
            "data_offsets":[offset, offset + bytes]}),
        );
        offset += bytes;
    }
    let mut header = serde_json::to_vec(&header).unwrap();
    while header.len() % 8 != 0 {
        header.push(b' ');
    }
    let mut file = fs::File::create(directory.join("model.safetensors")).unwrap();
    file.write_all(&(header.len() as u64).to_le_bytes())
        .unwrap();
    file.write_all(&header).unwrap();
    file.write_all(&vec![0; offset as usize]).unwrap();
}

fn run(work: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOOM_MODELS", work.join("models"))
        .env("LOOM_MODEL_STATE_DIR", work.join("state"))
        .args(args)
        .output()
        .unwrap()
}

fn report(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn import(work: &Path, source: &Path, output: &Path) -> Output {
    // macOS temp paths start at the /var symlink; the importer intentionally
    // requires a source with no symlink components.
    let source = fs::canonicalize(source).unwrap();
    run(
        work,
        &[
            "model",
            "import",
            source.to_str().unwrap(),
            "--native",
            "--output",
            output.to_str().unwrap(),
            "--json",
        ],
    )
}

#[test]
fn native_dense_and_moe_import_inspect_without_installing_or_claiming_runtime() {
    for moe in [false, true] {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("mlx");
        let output = work.path().join("native.lmx");
        mlx_fixture(&source, moe);
        let imported = report(import(work.path(), &source, &output));
        assert_eq!(&fs::read(&output).unwrap()[..8], b"LOOMLM2\0");
        assert_eq!(imported["format"], "lmx");
        assert_eq!(imported["version"], 2);
        assert_eq!(imported["native"], true);
        assert_eq!(imported["bytes"], fs::metadata(&output).unwrap().len());
        assert_eq!(imported["identity"].as_str().unwrap().len(), 64);
        assert_eq!(imported["tensors"], if moe { 18 } else { 14 });
        assert_eq!(imported["expert_blocks"], if moe { 2 } else { 0 });
        assert_eq!(imported["experts"], if moe { 2 } else { 0 });
        assert_eq!(imported["active_experts"], if moe { 1 } else { 0 });
        assert_eq!(imported["runtime_ready"], false);
        assert!(
            imported["runtime_reason"]
                .as_str()
                .unwrap()
                .contains("native runtime hookup pending")
        );

        let inspected = report(run(
            work.path(),
            &[
                "model",
                "inspect",
                output.to_str().unwrap(),
                "--verify",
                "--json",
            ],
        ));
        for key in [
            "identity",
            "model",
            "tensors",
            "expert_blocks",
            "experts",
            "bytes",
            "runtime_ready",
        ] {
            assert_eq!(inspected[key], imported[key], "{key}");
        }
        assert_eq!(
            inspected["model"]["architecture"],
            if moe { "qwen3_moe" } else { "qwen3" }
        );
        assert_eq!(inspected["payload_verified"], true);
        let listing = run(work.path(), &["model", "list"]);
        assert!(
            listing.status.success(),
            "{}",
            String::from_utf8_lossy(&listing.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&listing.stdout)
                .contains(imported["identity"].as_str().unwrap())
        );
        assert!(!work.path().join("models/lmx").exists());

        let text = run(work.path(), &["model", "inspect", output.to_str().unwrap()]);
        assert!(text.status.success());
        let text = String::from_utf8_lossy(&text.stdout);
        assert!(text.contains("LMX v2"));
        assert!(text.contains("native runtime hookup pending"));
    }
}

#[test]
fn native_import_never_overwrites_destination() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mlx");
    mlx_fixture(&source, false);
    let output = work.path().join("existing.lmx");
    fs::write(&output, b"keep existing model").unwrap();
    let failed = import(work.path(), &source, &output);
    assert!(!failed.status.success());
    assert_eq!(fs::read(&output).unwrap(), b"keep existing model");
    assert!(String::from_utf8_lossy(&failed.stderr).contains("exist"));
}

#[test]
fn native_inspect_rejects_invalid_version_and_truncated_native_without_v1_fallback() {
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("malformed.lmx");
    let mut header = vec![0u8; 128];
    header[..8].copy_from_slice(b"LOOMLM2\0");
    header[8..12].copy_from_slice(&99u32.to_le_bytes());
    fs::write(&path, header).unwrap();
    let failed = run(
        work.path(),
        &["model", "inspect", path.to_str().unwrap(), "--json"],
    );
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("unsupported native LMX version"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    fs::write(&path, b"LOOMLM2\0").unwrap();
    let failed = run(
        work.path(),
        &["model", "inspect", path.to_str().unwrap(), "--verify"],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("truncated native LMX header"));
}

#[test]
fn native_inspect_verify_authenticates_payloads() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mlx");
    let output = work.path().join("native.lmx");
    mlx_fixture(&source, false);
    report(import(work.path(), &source, &output));
    let native = loom_core::models::lmx_native_file::NativeFile::open(&output).unwrap();
    let block = native.manifest.blocks.values().next().unwrap();
    let mut file = fs::OpenOptions::new().write(true).open(&output).unwrap();
    file.seek(SeekFrom::Start(block.offset)).unwrap();
    file.write_all(&[0xff]).unwrap();
    let inspected = report(run(
        work.path(),
        &["model", "inspect", output.to_str().unwrap(), "--json"],
    ));
    assert_eq!(inspected["payload_verified"], false);
    let failed = run(
        work.path(),
        &[
            "model",
            "inspect",
            output.to_str().unwrap(),
            "--verify",
            "--json",
        ],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("checksum"));
}

#[test]
fn native_flags_require_explicit_mode_and_destination_before_importing() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mlx");
    let output = work.path().join("native.lmx");
    mlx_fixture(&source, false);
    for options in [
        vec!["--native"],
        vec!["--output", output.to_str().unwrap()],
        vec!["--native", "--output"],
        vec![
            "--native",
            "--output",
            output.to_str().unwrap(),
            "--unexpected",
        ],
    ] {
        let mut args = vec!["model", "import", source.to_str().unwrap()];
        args.extend(options);
        assert!(!run(work.path(), &args).status.success());
        assert!(!output.exists());
    }
}

#[test]
fn native_gguf_import_and_inspect_through_product_cli() {
    let work = tempfile::tempdir().unwrap();
    let gguf_path = work.path().join("source.gguf");
    let lmx_path = work.path().join("converted.lmx");

    // Build synthetic Qwen3 GGUF fixture
    let arch = "qwen3";
    let mut buf = b"GGUF".to_vec();
    buf.extend(3u32.to_le_bytes());

    fn write_string(buf: &mut Vec<u8>, s: &str) {
        buf.extend((s.len() as u64).to_le_bytes());
        buf.extend(s.as_bytes());
    }
    fn write_kv_u32(buf: &mut Vec<u8>, key: &str, val: u32) {
        write_string(buf, key);
        buf.extend(4u32.to_le_bytes());
        buf.extend(val.to_le_bytes());
    }
    fn write_kv_f32(buf: &mut Vec<u8>, key: &str, val: f32) {
        write_string(buf, key);
        buf.extend(6u32.to_le_bytes());
        buf.extend(val.to_le_bytes());
    }
    fn write_kv_str(buf: &mut Vec<u8>, key: &str, val: &str) {
        write_string(buf, key);
        buf.extend(8u32.to_le_bytes());
        write_string(buf, val);
    }
    fn write_kv_arr_str(buf: &mut Vec<u8>, key: &str, arr: &[&str]) {
        write_string(buf, key);
        buf.extend(9u32.to_le_bytes());
        buf.extend(8u32.to_le_bytes());
        buf.extend((arr.len() as u64).to_le_bytes());
        for s in arr {
            write_string(buf, s);
        }
    }

    let tensors: Vec<(&'static str, Vec<u64>)> = vec![
        ("token_embd.weight", vec![32, 32]),
        ("output_norm.weight", vec![32]),
        ("blk.0.attn_norm.weight", vec![32]),
        ("blk.0.ffn_norm.weight", vec![32]),
        ("blk.0.attn_q_norm.weight", vec![32]),
        ("blk.0.attn_k_norm.weight", vec![32]),
        ("blk.0.attn_q.weight", vec![32, 32]),
        ("blk.0.attn_k.weight", vec![32, 32]),
        ("blk.0.attn_v.weight", vec![32, 32]),
        ("blk.0.attn_output.weight", vec![32, 32]),
        ("blk.0.ffn_gate.weight", vec![32, 32]),
        ("blk.0.ffn_up.weight", vec![32, 32]),
        ("blk.0.ffn_down.weight", vec![32, 32]),
    ];

    buf.extend((tensors.len() as u64).to_le_bytes());
    buf.extend(15u64.to_le_bytes());

    write_kv_str(&mut buf, "general.architecture", arch);
    write_kv_u32(&mut buf, "general.vocab_size", 32);
    write_kv_u32(&mut buf, &format!("{arch}.embedding_length"), 32);
    write_kv_u32(&mut buf, &format!("{arch}.block_count"), 1);
    write_kv_u32(&mut buf, &format!("{arch}.attention.head_count"), 1);
    write_kv_u32(&mut buf, &format!("{arch}.attention.head_count_kv"), 1);
    write_kv_u32(&mut buf, &format!("{arch}.attention.key_length"), 32);
    write_kv_u32(&mut buf, &format!("{arch}.context_length"), 128);
    write_kv_u32(&mut buf, &format!("{arch}.feed_forward_length"), 32);
    write_kv_f32(&mut buf, &format!("{arch}.rope.freq_base"), 10000.0);
    write_kv_f32(&mut buf, &format!("{arch}.attention.layer_norm_rms_epsilon"), 1e-6);

    let vocab: Vec<&str> = (0..32).map(|_| "token").collect();
    write_kv_arr_str(&mut buf, "tokenizer.ggml.tokens", &vocab);
    write_kv_arr_str(&mut buf, "tokenizer.ggml.merges", &[]);
    write_kv_u32(&mut buf, "tokenizer.ggml.bos_token_id", 0);
    write_kv_u32(&mut buf, "tokenizer.ggml.eos_token_id", 1);

    let mut offset = 0u64;
    let mut raw_data = Vec::new();
    for (name, ne) in &tensors {
        write_string(&mut buf, name);
        buf.extend((ne.len() as u32).to_le_bytes());
        for &dim in ne {
            buf.extend(dim.to_le_bytes());
        }
        buf.extend(0u32.to_le_bytes()); // GGML_TYPE_F32
        buf.extend(offset.to_le_bytes());
        let bytes = (ne.iter().product::<u64>() * 4) as usize;
        raw_data.push(vec![0u8; bytes]);
        let aligned = (bytes + 31) / 32 * 32;
        offset += aligned as u64;
    }

    while buf.len() % 32 != 0 {
        buf.push(0);
    }
    for d in raw_data {
        buf.extend_from_slice(&d);
        while buf.len() % 32 != 0 {
            buf.push(0);
        }
    }
    fs::write(&gguf_path, buf).unwrap();

    let output = run(
        work.path(),
        &[
            "model",
            "import",
            gguf_path.to_str().unwrap(),
            "--native",
            "--output",
            lmx_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let inspected = report(run(
        work.path(),
        &[
            "model",
            "inspect",
            lmx_path.to_str().unwrap(),
            "--verify",
            "--json",
        ],
    ));
    assert_eq!(inspected["format"], "lmx");
    assert_eq!(inspected["native"], true);
    assert_eq!(inspected["version"], 2);
    assert_eq!(inspected["payload_verified"], true);
    assert_eq!(inspected["model"]["architecture"], "qwen3");
    assert_eq!(inspected["model"]["hidden"], 32);
}

#[test]
fn native_gguf_q6k_import_and_inspect_through_product_cli() {
    let work = tempfile::tempdir().unwrap();
    let gguf_path = work.path().join("source_q6k.gguf");
    let lmx_path = work.path().join("converted_q6k.lmx");

    let arch = "qwen3";
    let mut buf = b"GGUF".to_vec();
    buf.extend(3u32.to_le_bytes());

    fn write_string(buf: &mut Vec<u8>, s: &str) {
        buf.extend((s.len() as u64).to_le_bytes());
        buf.extend(s.as_bytes());
    }
    fn write_kv_u32(buf: &mut Vec<u8>, key: &str, val: u32) {
        write_string(buf, key);
        buf.extend(4u32.to_le_bytes());
        buf.extend(val.to_le_bytes());
    }
    fn write_kv_f32(buf: &mut Vec<u8>, key: &str, val: f32) {
        write_string(buf, key);
        buf.extend(6u32.to_le_bytes());
        buf.extend(val.to_le_bytes());
    }
    fn write_kv_str(buf: &mut Vec<u8>, key: &str, val: &str) {
        write_string(buf, key);
        buf.extend(8u32.to_le_bytes());
        write_string(buf, val);
    }
    fn write_kv_arr_str(buf: &mut Vec<u8>, key: &str, arr: &[&str]) {
        write_string(buf, key);
        buf.extend(9u32.to_le_bytes());
        buf.extend(8u32.to_le_bytes());
        buf.extend((arr.len() as u64).to_le_bytes());
        for s in arr {
            write_string(buf, s);
        }
    }

    let tensors: Vec<(&'static str, Vec<u64>, bool)> = vec![
        ("token_embd.weight", vec![256, 256], true),
        ("output_norm.weight", vec![256], false),
        ("blk.0.attn_norm.weight", vec![256], false),
        ("blk.0.ffn_norm.weight", vec![256], false),
        ("blk.0.attn_q_norm.weight", vec![256], false),
        ("blk.0.attn_k_norm.weight", vec![256], false),
        ("blk.0.attn_q.weight", vec![256, 256], true),
        ("blk.0.attn_k.weight", vec![256, 256], true),
        ("blk.0.attn_v.weight", vec![256, 256], true),
        ("blk.0.attn_output.weight", vec![256, 256], true),
        ("blk.0.ffn_gate.weight", vec![256, 256], true),
        ("blk.0.ffn_up.weight", vec![256, 256], true),
        ("blk.0.ffn_down.weight", vec![256, 256], true),
    ];

    buf.extend((tensors.len() as u64).to_le_bytes());
    buf.extend(15u64.to_le_bytes());

    write_kv_str(&mut buf, "general.architecture", arch);
    write_kv_u32(&mut buf, "general.vocab_size", 256);
    write_kv_u32(&mut buf, &format!("{arch}.embedding_length"), 256);
    write_kv_u32(&mut buf, &format!("{arch}.block_count"), 1);
    write_kv_u32(&mut buf, &format!("{arch}.attention.head_count"), 1);
    write_kv_u32(&mut buf, &format!("{arch}.attention.head_count_kv"), 1);
    write_kv_u32(&mut buf, &format!("{arch}.attention.key_length"), 256);
    write_kv_u32(&mut buf, &format!("{arch}.context_length"), 128);
    write_kv_u32(&mut buf, &format!("{arch}.feed_forward_length"), 256);
    write_kv_f32(&mut buf, &format!("{arch}.rope.freq_base"), 10000.0);
    write_kv_f32(&mut buf, &format!("{arch}.attention.layer_norm_rms_epsilon"), 1e-6);

    let vocab: Vec<&str> = (0..256).map(|_| "token").collect();
    write_kv_arr_str(&mut buf, "tokenizer.ggml.tokens", &vocab);
    write_kv_arr_str(&mut buf, "tokenizer.ggml.merges", &[]);
    write_kv_u32(&mut buf, "tokenizer.ggml.bos_token_id", 0);
    write_kv_u32(&mut buf, "tokenizer.ggml.eos_token_id", 1);

    let mut offset = 0u64;
    let mut raw_data = Vec::new();
    for (name, ne, is_q6k) in &tensors {
        write_string(&mut buf, name);
        buf.extend((ne.len() as u32).to_le_bytes());
        for &dim in ne {
            buf.extend(dim.to_le_bytes());
        }
        let kind = if *is_q6k { 14u32 } else { 0u32 }; // GGML_TYPE_Q6_K (14) or F32 (0)
        buf.extend(kind.to_le_bytes());
        buf.extend(offset.to_le_bytes());
        let values: u64 = ne.iter().product();
        let bytes = if *is_q6k {
            (values / 256 * 210) as usize
        } else {
            (values * 4) as usize
        };
        raw_data.push(vec![0u8; bytes]);
        let aligned = (bytes + 31) / 32 * 32;
        offset += aligned as u64;
    }

    while buf.len() % 32 != 0 {
        buf.push(0);
    }
    for d in raw_data {
        buf.extend_from_slice(&d);
        while buf.len() % 32 != 0 {
            buf.push(0);
        }
    }
    fs::write(&gguf_path, buf).unwrap();

    let output = run(
        work.path(),
        &[
            "model",
            "import",
            gguf_path.to_str().unwrap(),
            "--native",
            "--output",
            lmx_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let inspected = report(run(
        work.path(),
        &[
            "model",
            "inspect",
            lmx_path.to_str().unwrap(),
            "--verify",
            "--json",
        ],
    ));
    assert_eq!(inspected["format"], "lmx");
    assert_eq!(inspected["native"], true);
    assert_eq!(inspected["version"], 2);
    assert_eq!(inspected["payload_verified"], true);
    assert_eq!(inspected["model"]["architecture"], "qwen3");
    assert_eq!(inspected["model"]["hidden"], 256);
}
