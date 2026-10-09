# 핸드오프 — LMX 단계 C (GGUF 양자화를 MLX에서 실행), 2026-10-09

> **2026-10-09 후속 결정:** 사용자가 LMX를 GGUF/MLX 컨테이너가 아닌 Loom 전용 모델 포맷으로 재정의했다. 새 구현의 기준은 [LMX 전용 모델 파일과 실행 계약](loom-lmx.md)이다. 아래는 이전 v1 실험의 핸드오프이며 native LMX의 완료 조건으로 사용하지 않는다. 후속 메모리 실험에서 변환 단독 footprint 증가량은 713,212,432 bytes(최종 가중치 709,000,960 bytes)로 줄었지만, native schema/reader/writer와 독립 tokenizer·CPU/CUDA 실행기는 아직 구현되지 않았다.

## 상태: 동작 확인됨, 메모리 기준 미달 1건

GGUF payload를 담은 LMX를 MLX 엔진이 직접 실행함. 변환은 **로드 시 메모리 안에서** 텐서 하나씩 수행.
디스크 부산물이나 외부 파일은 없음: LMX 한 파일이 tokenizer, config, template, weights를 모두 가짐.
LMX 원본 GGUF payload는 바뀌지 않음 (llama.cpp도 같은 파일을 그대로 실행).

## 구현 (모두 미커밋)

| 파일 | 내용 |
|---|---|
| `loom-core/src/models/gguf_mlx.rs` (신규) | GGUF 블록 → MLX affine(group 32). 상세는 표 아래 |
| `loom-core/src/models/mod.rs` | `pub mod gguf_mlx;` |
| `loom-core/native/mlx/engine.cpp` | 모델 조립을 `build_model()`로 분리, 증분 API 추가. 상세는 표 아래 |
| `loom-core/src/models/mlx_runtime.rs` | `GgufSource`(tensor 목록, template, `load()`), `MlxModel::from_gguf`, `Vocab` enum. 상세는 표 아래 |
| `loom-core/src/models/native_runtime.rs` | `llama_backend()`를 `pub(super)`로 |
| `loom-core/src/models/lmx_runtime.rs` | `capability()`에 MLX+GGUF 분기 추가. 상세는 표 아래 |
| `custom-tui/tests/lmx_cli.rs` | 거부 사유 기대값을 `"dummy has no MLX name"`으로 변경 (fixture 텐서 이름이 `dummy`) |

**`gguf_mlx.rs`**
- 지원 타입: q8_0, q4_0, q4_1, q5_0, q5_1, q4_K, q5_K (+ dense f32/f16/bf16는 F16으로 변환).
- level × scale + bias 형식. 비트스트림은 little-endian으로 pack. scales/biases는 f16.
- `hf_name()`: GGUF 이름을 HF 정식 이름으로 매핑 (LoRA 어댑터 호환 목적).
- `read_config()`: GGUF 메타데이터에서 config와 chat template을 읽음.
- `unsupported()`: q6_K/q3_K/q2_K는 "16-value groups" 사유로 거부.
- 단위 테스트 3개: llama.cpp 역양자화 참조 구현과 값 비교, 레거시 타입은 정확히 일치. K-quant는 f16 반올림 1회 오차.

**`engine.cpp`**
- 모델 조립을 `build_model()`로 분리.
- 증분 API: `loom_mlx_weights_new/add/free` 및 `loom_mlx_load_weights`.

**`mlx_runtime.rs`**
- `GgufSource`: tensor 목록과 template을 들고, `load()`는 텐서 하나씩 읽기 → 변환 → 추가.
- `MlxModel::from_gguf`: architecture는 qwen2/qwen3만. lm_head가 없으면 tied로 처리.
- `Vocab` enum:
  - `Hf(tokenizers)`
  - `Gguf{llama.cpp vocab_only 모델, eog 목록}` — 같은 LMX 파일에서 로드.
- 종료 판정: `model.eos` 또는 `is_eog`.

**`lmx_runtime.rs` `capability()` MLX+GGUF 분기**
- 모든 텐서가 HF 이름을 갖고 타입이 지원되며 architecture가 `gguf_mlx::ARCHITECTURES`(qwen2, qwen3)에 속하면 통과.
- 무시되는(ignored) parity 테스트 `gguf_on_mlx_matches_llama_cpp` 추가.

## 검증 결과

모델: Qwen2.5-0.5B-Instruct q8_0 GGUF → LMX (`dense:F32, ggml:q8_0`, 291 tensors).

**Greedy 32토큰 parity (CPU llama.cpp vs MLX)**
- 영어 프롬프트 125/125자, 한국어 프롬프트 42/42자 완전 일치.
- 실행:
  `LOOM_LMX_PARITY=/path/model.lmx cargo test --release --lib gguf_on_mlx_matches_llama_cpp -- --ignored --nocapture`

**`loom model bench --prompt 512 --gen 128`**

| 엔진 | load | prefill | decode | peak footprint |
|---|---|---|---|---|
| cpu (llama.cpp) | 741 ms, +0.67 GB | 988 tok/s | 156 tok/s | 0.76 GB |
| mlx (변환) | 1249 ms, +1.17 GB | 2450 tok/s | 236 tok/s | **1.39 GB** |

**테스트**
- loom-core lib: 158 passed, 8 ignored.
- custom-tui `lmx_cli`: passed.
- 전체 `cargo test -- --test-threads=1`과 TUI `--self-check`는 **이번엔 미실행**. UI 변경이 없어서 생략했으나, 다음 세션에서 한번 돌릴 것.

## 남은 일 (우선순위 순)

1. **메모리 기준 미달 (완료 조건 위반)**
   - 증상: MLX load가 +1.17 GB. 예상치는 q8_0 8.5→9 bit/weight 기준 약 0.72 GB.
   - 의심 지점:
     - (a) `mx::array(const T*, shape, dtype)` 복사가 남기는 버퍼, 또는 MLX buffer cache;
     - (b) tied embed가 `embed`와 `head`로 복제되는지 (같은 array라 아닐 것으로 추정);
     - (c) Rust `Vec` 두 개 (source + converted)가 겹치는 피크.
   - 확인 방법: `loom_mlx_active_memory`를 텐서 추가 전후로 측정하고, 매 추가 후 `mx::eval` 및 `clear_cache`.
   - 대안: `allocator::malloc` 버퍼에 직접 pack (복사 0회).
2. **Auto 선택 정책**
   - 현재 Auto는 MLX를 먼저 고르므로, GGUF qwen2/qwen3 LMX가 이제 MLX로 실행됨.
   - 속도는 이득이지만 메모리가 1번 해결 전까지 더 큼.
   - 결정 필요: 1번 해결 후 유지하거나, 그 전까지 GGUF는 CPU 우선으로 둘지.
3. **q4_0 / q4_K 실모델 parity**
   - Q4_K_M 파일은 q6_K 텐서가 섞여 있어 MLX에서 거부됨 (정상 동작).
   - `--pure`로 양자화한 q4_0이나 q4_K 파일로 검증 필요.
4. q6_K, q3_K, q2_K: group 16 전용 Metal 커널 필요 (MLX `quantized_matmul`은 group 32/64만 지원).
5. llama 계열 GGUF: Q/K 행 역-permute 후 `ARCHITECTURES`에 추가. MoE (qwen3moe)는 이름 매핑만 있고 config는 없음.
6. `docs/loom-lmx.md`에 단계 C 결과 반영 (아직 안 함). 그다음은 단계 D (실행 계획: 장치와 메모리 통합).

## 제약 (유지)

- LMX는 **단독 파일**이어야 함. 외부 tokenizer, config, 캐시 파일 금지.
- LMX 목적은 도구 사용 LoRA 어댑터. 따라서:
  - tensor 이름은 HF 정식 이름 유지;
  - base weights는 그대로 두고 어댑터는 분리.
- 엔진 파일을 수정하기 전에 mtime 확인 (다른 세션과 동시 편집 주의).
- 답변은 한국어로. 메모리 우선, 다음이 속도.

## 임시 파일

- scratchpad `gguf/q8.gguf` (676 MB) 및 `models/` (LMX 0.68 GB).
- 세션 scratchpad라 자동 정리되지만, 필요하면 수동 삭제.
