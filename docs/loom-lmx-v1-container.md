# LMX v1 컨테이너 — 과거 구현과 실험 기록

> 2026-10-09 목표 재정의로 이 문서의 v1 컨테이너 설계는 최종 LMX 규격에서 제외됐다. 현재 코드를 설명하는 기록이며, 새 구현의 기준은 [LMX 전용 모델 포맷](loom-lmx.md)이다. 아래 구현·측정 결과를 native LMX의 완성으로 해석하지 않는다.

2026-10-09. LMX는 원본 가중치 표현을 보존하는 단일 모델 파일이다. 가져오기·검증·모델 식별·엔진 선택·일반 요청과 스트리밍의 진입점을 공통으로 제공한다. 현재 수치 실행기는 자체 MLX와 llama.cpp 기반 CPU/CUDA다.

## 이 구현의 한계와 남은 의존성

목표는 LMX 한 파일을 Loom 전용 실행기가 읽고 **Metal → CUDA → CPU** 순서로 장치에 맞는 자체 커널을 선택하는 것이다. 원본 GGUF payload를 보존하는 것은 가중치 바이트와 정확도를 지키기 위한 저장 방식이며, llama.cpp를 영구 실행기로 삼겠다는 의미가 아니다. tensor 역할과 HF 이름, 양자화 layout은 공통 IR과 Loom 실행기에서 해석해야 한다. 도구 사용 LoRA는 base weight와 분리한다.

현재 자체 구현은 MLX forward pass와 GGUF 블록 재배치다. 기존 CPU/CUDA 추론, GGUF metadata parser와 vocab-only tokenizer에는 llama.cpp 의존이 남아 있다. 전용 CPU/CUDA 커널과 독립 GGUF tokenizer/parser는 아직 구현되지 않았다. 따라서 이 버전은 전용 엔진으로 가는 중간 단계이며, 컨테이너/라우터를 만든 것만으로 전용 엔진이 완성됐다고 보지 않는다. llama.cpp Metal 실행기를 새 폴백으로 추가하지 않는다.

## 사용

```sh
loom model import /absolute/path/model.gguf --json
loom model import /absolute/path/mlx-checkpoint --json
loom model import /absolute/path/model.lmx --json
loom model pull hf:owner/repo/model.gguf
loom model pull mlx:owner/repo@revision
loom model list
loom model inspect lmx/<identity> --verify --json
loom model bench lmx/<identity> --engine cpu --ctx 512 --prompt 128 --gen 32 --json
loom model bench lmx/<identity> --engine mlx --prompt 128 --gen 32 --json
```

결과는 `LOOM_MODELS/lmx/<identity>.lmx`에 저장하고 일반 모델 라우터에도 등록한다. 기본 root는 `~/.loom/models`다. 이번 자동 준비 연결은 제품 CLI의 `model import/pull`에 적용했다. TUI 내부의 기존 GGUF 다운로드 관리자는 아직 기존 다운로드 경로를 사용한다. `LOOM_LMX_ENGINE=auto|cpu|mlx|cuda`는 일반 요청의 엔진을 선택한다. 벤치마크의 `--engine`이 환경변수보다 우선한다. 명시한 엔진이 없거나 저장 표현을 지원하지 않으면 오류를 반환한다.

## 현재 실행 범위

| LMX에 담긴 표현 | CPU | 자체 MLX (Metal) | CUDA |
| --- | --- | --- | --- |
| GGUF의 원래 tensor/block quantization | 기존 llama.cpp | Qwen2/Qwen3, 아래 지원 타입 | 기존 llama.cpp, `cuda` feature와 장치 필요 |
| 지원되는 MLX safetensors/affine | 미지원 | 자체 MLX | 미지원 |

엔진 선택은 공통 모델 IR의 capability와 실제 장치 가용성을 확인한다. `auto`는 **자체 MLX/Metal → 가용한 기존 CUDA → CPU** 순서다. `metal`/`mps`는 `mlx`의 별칭이며 llama.cpp Metal 폴백을 추가하지 않는다. 명시한 엔진은 다른 엔진으로 대체하지 않는다. 자체 CUDA 수치 실행기는 아직 구현되지 않았다.

`inspect`의 `payload_supported`, `compiled`, `available`은 각각 표현 지원, 빌드 포함 여부, 장치·드라이버 가용성을 구분한다. CUDA를 빌드했어도 장치가 없으면 Auto에서 건너뛴다. 이번 CUDA 선택 검사는 가용성을 모사한 단위 검사이며 CUDA 실기기 검증은 아니다. 기존 CPU/CUDA는 llama.cpp의 architecture/quantization 지원 범위를 따른다. 파일 구조를 검사했다고 해당 architecture의 실제 추론까지 확인된 것은 아니다.

MLX는 기존 Qwen2/Qwen3/Qwen3 MoE/Llama/Mistral 호환 구조와 FP16/BF16/FP32, affine 2/3/4/5/6/8bit 혼합 텐서를 받는다. 모델 안의 group size는 아직 동일해야 하며, MoE는 packed expert bank 형태가 필요하다. shared expert, sliding window, scaled/partial RoPE, GPTQ/AWQ 등의 추가 실행 지원은 포함되지 않았다.

GGUF payload는 디스크에서 그대로 유지하며 MLX 로드 시 지원하는 블록만 메모리 안에서 affine으로 재배치한다. 모든 GGUF 타입을 MLX에서 실행하는 공통 수치 포맷은 아직 아니다. CPU/CUDA의 자체 수치 커널은 새로 작성하지 않았다.

## 파일 구조

모든 정수는 little endian이다.

1. 원본 payload를 4,096-byte 경계에 배치한다. GGUF는 파일 시작 위치 0에 원본 전체를 그대로 둔다. MLX는 weight shard 다음에 config/tokenizer/template 등의 작은 파일을 둔다.
2. 정렬된 payload 뒤에 최대 32 MiB JSON index를 둔다. source, profile=`preserve`, 파일별 위치·길이·SHA-256, tensor와 양자화 descriptor를 기록한다.
3. 마지막 64 bytes는 magic `LOOMLMX\0`(8), version=1(4), flags=0(4), index offset(8), index length(8), index SHA-256(32)다.

identity는 index SHA-256이다. 출처와 계획도 index에 포함되므로 출처가 다른 두 원본은 가중치가 같아도 identity가 다를 수 있다. 해시는 손상 검사이며 서명이 아니다.

GGUF의 llama.cpp 실행기는 LMX의 원본 GGUF prefix를 직접 열어 기존 mmap 경로를 사용한다. GGUF의 MLX 실행기는 같은 파일에서 config/template을 읽고 llama.cpp의 vocab-only 모델로 tokenizer를 사용한다. 가중치는 64KiB 이하 블록을 읽어 최종 MLX 버퍼에 직접 변환하며, 외부 tokenizer/config나 변환 캐시 파일을 만들지 않는다.

기존 safetensors LMX 경로는 shard 범위를 제한한 reader로 가중치를 직접 읽고 tokenizer 등의 작은 asset은 `LOOM_MODELS/.lmx-cache/<identity>/metadata`에 복원한다. 이 기존 경로의 metadata 캐시 제거는 이번 GGUF 메모리 수정에 포함하지 않았다. MLX의 zero-copy mmap을 보장하는 구현도 아니다.

## 메모리·게시·검증

- 원본 표현과 바이트를 보존한다. import는 1 MiB 버퍼로 복사하며 전체 모델을 FP16으로 확장하거나 재양자화하지 않는다. 원본을 보관하므로 LMX만큼 디스크 사용량은 추가된다.
- 준비 작업은 기존 공통 자원 관리자에 256 MiB를 예약한다. 예약은 OS 강제 한도가 아니다. GGUF CPU/CUDA의 실행 메모리와 VRAM을 공통 예산에 넣는 작업은 남아 있다.
- 임시 파일에서 검증한 후 관리 root 안에서 원자적으로 게시한다. 취소·오류 시 임시 파일을 정리하고 원본은 보존한다. 동일 identity 캐시는 전체 payload SHA-256을 확인한 뒤 재사용한다.
- 열 때 footer/index 해시, 파일·tensor 범위, GGUF header 또는 실제 safetensors header와 descriptor의 일치를 검사한다. 전체 payload 해시는 import와 명시적인 `inspect --verify`에서 검사하며 매 추론마다 전체 가중치를 다시 읽지 않는다.
- metadata는 파일당 32 MiB, safetensors header는 8 MiB, tensor는 최대 50,000개다. split GGUF는 먼저 병합해야 한다.
- 기존 다운로드의 revision 고정·재개·검증을 재사용하고, 내려받은 원본도 남긴다. 기존 Rust MLX directory import API와 기존 모델 ID는 유지한다. `model remove`의 LMX 삭제 지원은 아직 없다.

## 공통 모델 IR (단계 A·B — 구현됨)

`loom-core/src/models/lmx_ir.rs`. 모든 LMX 모델을 같은 형태로 기술한다: 텐서마다 담긴 entry, entry 안의 offset/bytes, **논리 shape**(양자화 전, 바깥 차원 먼저), 저장 표현.

| 저장 표현 | 의미 | 출처 |
| --- | --- | --- |
| `Dense { dtype }` | 원소당 값 하나(F32/F16/BF16/U32 …) | GGUF block size 1, safetensors 일반 텐서 |
| `MlxAffine { bits, group_size, axis, scales, biases }` | 마지막 축 group affine, scale/bias는 이름으로 참조하는 Dense 텐서 | MLX descriptor |
| `GgmlBlock { type_name, block_size, type_size }` | ggml 블록 양자화(q4_0, q4_K, q6_K …) | GGUF header |

- **저장하지 않는다.** v1 index는 그대로이고, 열 때마다 검증용으로 이미 읽는 GGUF header·MLX descriptor에서 다시 만든다. 그래서 기존 v1 파일이 그대로 열리고, 가중치를 읽거나 복사하지 않는다. 실제 열기 시간은 변경 전과 같았다(Q6_K GGUF 0.06s, 반복 측정).
- GGUF `ne`는 가장 빠른 차원이 먼저라서 뒤집고, 뒤쪽 크기 1 차원(패딩)은 버린다. MLX affine 가중치는 저장된 U32 packed shape에서 `last × 32 / bits`로 논리 shape를 구한다.
- **검증:** entry 범위 안인지, 크기 0 차원이 없는지, dtype 크기 × shape = bytes, affine은 bits·group·packed bytes·scale/bias 참조의 존재와 shape, ggml 블록은 블록 수 × type_size = bytes인지 검사한다. 하나라도 틀리면 열기가 실패한다.
- **Capability:** `lmx_runtime::capability(ir, engine)`이 모델이 요구하는 것(payload 형식, architecture, 모든 저장 표현)을 엔진 능력과 비교해 이유를 돌려준다. 빌드 포함 여부(`compiled`)는 별도로 보고한다. `loom model inspect`가 architecture·텐서 수·가중치 크기·저장 표현 목록을 같이 보여준다.

## 이전 단계와 완료 조건 (native LMX의 완료 기준으로 사용하지 않음)

| 단계 | 상태 | 완료 조건 |
| --- | --- | --- |
| A. 공통 IR | **완료** | ① 기존 v1 파일이 그대로 열림 ② GGUF·MLX 모두 IR 생성 ③ shape·bytes·양자화 참조 검증 ④ 기존 CPU/MLX 출력 불변 ⑤ 가중치 추가 복사 없음 |
| B. 실행 capability | **완료** | ① 엔진 선택이 IR 기반 ② 실행 불가 시 구체적 이유 ③ 지원하지 않는 양자화를 지원한다고 표시하지 않음 ④ 명시한 엔진은 대체하지 않음 |
| C. GGUF 양자화의 MLX 실행 | **부분 구현·검증 중** | ① 최소 q4_0·q8_0·q4_K·q6_K를 FP16 전체 확장 없이 실행 ② 같은 모델의 llama.cpp CPU 출력과 greedy 32토큰 일치(또는 측정된 허용 오차) ③ 피크 메모리 ≤ 가중치 + KV + 측정된 작업 버퍼 ④ capability가 해당 타입을 MLX 지원으로 표시 |
| D. 실행 계획 통합 | 계획 | ① 장치·가용 메모리·연산 지원을 함께 보고 엔진/배치 결정 ② 자원 관리자 예산과 연결 ③ 결정 근거를 `inspect`에 표시 |

## 검증 기록 (단계 A·B, 2026-10-09)

변경 **전** 바이너리로 만든 v1 LMX 3개를 변경 **후** 코드로 열었다(임시 `LOOM_MODELS`, 사용자 모델 미변경).

| 모델 | IR | 검증 텐서 | 엔진 판정 |
| --- | --- | --- | --- |
| Qwen3.8-4B Q6_K GGUF | `qwen35`, dense:F32, ggml:q6_K | 441 | CPU 가능, MLX 불가(q6_K 커널 없음) |
| Qwen2.5-0.5B Q4_K_M GGUF | `qwen2`, F32·q4_K·q5_0·q6_K·q8_0 | 291 | CPU 가능 |
| Qwen3-4B 4bit MLX | `qwen3`, dense:BF16, mlx_affine:4 | 904 | MLX 가능, CPU/CUDA 불가 |

- 같은 프롬프트의 응답이 변경 전후 동일했다(MLX 응답 SHA-256 앞자리 `b5f6844b8599` 일치, Qwen2.5 GGUF 결과 일치). Qwen3.8(hybrid)은 CPU에서 시간 제한 안에 끝나지 않는 것이 변경 전후 같았다.
- Core 397 passed/0 failed/9 ignored, 제품 회귀 통과(기존 문구를 확인하던 LMX CLI 검사 1개는 새 구체적 사유로 기대값 수정), 자가검사 788 passed.

## 단계 C 구현 및 메모리 수정 (2026-10-09)

`gguf_mlx.rs`는 q8_0, q4_0, q4_1, q5_0, q5_1, q4_K, q5_K를 MLX affine group 32로 재배치한다. dense F32/F16/BF16은 F16으로 변환한다. K-quant의 sub-block scale/bias 곱은 F16으로 한 번 반올림하므로 원본 역양자화와 작은 차이가 있다. q6_K/q3_K/q2_K는 group 16 전용 커널이 없어 MLX capability에서 거부한다. GGUF architecture는 현재 qwen2/qwen3만 지원한다.

실행 시 tensor 이름은 HF 정식 이름으로 매핑한다. LMX 원본 payload와 base weight는 변경하지 않으며 LoRA 어댑터는 분리하는 계약을 유지한다. Llama 계열은 Q/K 행 역-permute, MoE는 config 지원을 추가해야 한다.

이전 로더는 텐서 전체의 Rust source Vec, 변환 Vec, MLX 복사본이 겹쳤다. 해제된 CPU 버퍼도 macOS 할당기가 보유해 footprint가 줄지 않았다. 텐서 복사 직후 `eval`과 MLX `clear_cache`로도 이 CPU 메모리는 해소되지 않았다. 수정한 로더는 `loom_mlx_weights_alloc`으로 최종 MLX 버퍼를 확보하고, 64KiB 이하 원본 블록을 읽어 그 버퍼에 직접 pack한다. 오류 시 pending weights가 버퍼를 회수한다.

Qwen2.5-0.5B-Instruct q8_0 LMX의 변환만 별도 프로세스로 측정했다. tokenizer와 첫 Metal 실행을 제외한 수치다.

| 항목 | bytes |
| --- | ---: |
| 계산된 packed weight + scales/biases | 709,000,960 |
| MLX active 증가량 및 peak active 증가량 | 711,405,312 |
| 프로세스 footprint 증가량 및 peak 증가량 | 713,212,432 |

변환 자체는 최종 가중치 외에 약 4.2MB의 실측 overhead였다. ignored 회귀 검사 `gguf_mlx_loading_has_no_tensor_sized_cpu_copies`는 최종 가중치 + 64MiB 이내인지 검사한다. 실제 추론 전체의 peak는 tokenizer/runtime, KV, 임시 연산 버퍼, MLX 재사용 캐시를 추가로 포함한다. 로드 벤치마크도 첫 1토큰 warmup을 포함하므로 이 표의 변환 단독 수치와 다르다.

이 세션의 측정 자료는 `/tmp/loom-lmx-stage-c-memory/`에 있다. 임시 자료이므로 영구 벤치마크 기록으로 보지 않는다. Q8의 128 생성 토큰은 수정 전후 동일했다. 동일 LMX의 GGUF prefix SHA-256도 원본 GGUF와 일치하며 모델 root에는 LMX 한 파일만 존재했다.

Greedy 최대 32토큰의 CPU/MLX 비교:

| 실모델 표현 | 영어 | 한국어 | 판정 |
| --- | --- | --- | --- |
| q8_0 | 125/125자 일치 | 42/42자 일치 | 이전 핸드오프에서 확인 |
| q4_0 (`pure`) | 143/143자 일치 | 공통 prefix 49/60자, 이후 불일치 | **미해결** |
| q4_K + q5_0 | 130/130자 일치 | 40/40자 일치 | 해당 혼합 모델 통과 |

q4_K 검증 파일은 `pure` 옵션으로 만들었지만 hidden size 896이 256으로 나뉘지 않아 llama.cpp가 146/291 텐서에 q5_0 fallback을 적용했다. 순수 q4_K 실모델 전체 검증으로 보지 않는다. q4_0 불일치의 원인은 아직 확인하지 않았다. 기존 ignored parity 검사는 빈 응답만 검사했으나 이제 CPU/MLX 문자열 전체 일치를 요구하므로 q4_0 한국어 사례는 실패로 드러난다. 허용 오차를 임의로 정해 통과시키지 않는다.

q6_K 미지원과 q4_0 불일치가 남아 있으므로 단계 C 전체 완료를 주장하지 않는다.

```sh
LOOM_LMX_PARITY=/path/model.lmx cargo test --release --manifest-path loom-core/Cargo.toml --lib gguf_mlx_loading_has_no_tensor_sized_cpu_copies -- --ignored --nocapture
LOOM_LMX_PARITY=/path/model.lmx cargo test --release --manifest-path loom-core/Cargo.toml --lib gguf_on_mlx_matches_llama_cpp -- --ignored --nocapture
```

## IR 미확정 사항

1. **[n] 과 [n, 1] 구분:** GGUF `ne`의 뒤쪽 1을 버리므로 두 shape이 같아진다. 실행에 필요해지면 `gguf_get_n_dims`류 정보를 함께 기록해야 한다.
2. **텐서 이름:** 원본 이름을 그대로 쓴다(GGUF `blk.0.attn_q.weight`, HF `model.layers.0.self_attn.q_proj.weight`). 엔진 무관 실행 계획에는 공통 역할(q_proj 등) 매핑이 필요하다.
3. **연산 그래프:** IR은 architecture 문자열까지만 가진다. attention·MoE·RoPE 변형 같은 연산 의미와 하이퍼파라미터(GGUF metadata, config.json)는 아직 IR 밖에 있다.
4. **group size 혼합:** IR은 텐서마다 group size를 담지만 MLX 엔진은 모델 하나에 한 값을 가정한다. capability가 아직 이를 검사하지 않는다.
5. **affine axis:** 마지막 축(-1)만 허용한다.
6. **저장 여부:** IR을 v2 index에 저장할지(외부 도구·빠른 열기) 계속 파생할지 정하지 않았다. 지금 측정으로는 파생 비용이 없다.
7. **드문 ggml 타입:** block size 1인 ggml 타입은 모두 Dense로 보며, dtype 크기를 모르면 검증에서 실패한다(예: 새 정수 타입).

## 다음 구현

1. 단계 C: q4_0 한국어 greedy 불일치의 logits/누적 오차를 조사하고, q6_K/q3_K/q2_K의 16-value group 커널을 구현한다. q4_K는 모든 행이 256값 블록으로 나뉘는 실모델에서도 검증한다.
2. 단계 D: GPU 우선 정책 안에서 장치·가용 메모리·KV·임시 버퍼를 함께 계획하고 기존 CPU/CUDA의 예산을 공통 자원 관리자에 연결한다.
3. architecture와 per-tensor quantization 지원을 확대하면서 Windows/Linux/CUDA 빌드·실기기 회귀를 병행한다. 이미지 입력은 별도 단계로 유지한다.

## 이전 검증 (LMX v1)

실제 모델과 회귀 검증 결과는 [LMX 측정 기록](../bench/mlx-memory-moe/lmx-v1.json)에 기록한다.

- Qwen3 계열 GGUF Q6_K와 MLX Qwen3-4B-4bit 각각에서 원본과 LMX의 128-token 입력 뒤 greedy 생성 32개 토큰이 모두 일치했다.
- 일반 Loom worker에서도 LMX 모델을 지정해 실제 응답을 받았다. 로컬 모델 호출 1회, 원격 호출 0회였다.
- MLX 가져오기 maximum RSS는 164,610,048 bytes, peak physical footprint는 121,160,304 bytes였다. 프로세스 한 개의 관측이며 전체 시스템 파일 캐시와 도구를 합친 메모리는 아니다.
- 실행 캐시에는 metadata 11,433,297 bytes만 생성됐고 GGUF/safetensors 가중치 사본은 없었다.
- Core 회귀 194 passed/8 ignored, 제품 회귀 329 passed/3 ignored, 제품 자가검사 785 passed/0 failed. LMX 최종 집중 검사 6개와 제품 CLI 검사 1개도 통과했다. native MoE 72개 조합과 LMX slice의 F16/Q4 KV 검사를 activation compile 켬/끔 양쪽에서 통과했다.
- MLX runtime을 의도적으로 제외한 Core 빌드 검사도 통과했다. Windows/Linux와 CUDA 실기기 검증을 대신하는 결과는 아니다.

포맷 통합 자체의 속도 향상은 주장하지 않는다. 실제 모델 검사는 백그라운드 빌드·테스트 및 다른 작업과 일부 겹쳤다. 검증용 LMX는 임시 관리 root에서 만들었고 사용자 원본은 수정하지 않았다.
