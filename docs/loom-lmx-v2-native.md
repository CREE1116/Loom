# Native LMX v2 — 파일 규격과 MoE 페이지 경계

2026-10-09. 구현: `loom-core/src/models/lmx_native.rs`, `lmx_native_file.rs`, `lmx_native_import.rs`, `lmx_expert_cache.rs`.

v2는 Loom 모델 정의·정규화된 tensor·가중치·tokenizer·template을 저장한다. GGUF나 safetensors 파일 전체, 원본 config를 payload로 저장하지 않는다. reader는 원본 parser를 호출하지 않는다. v1 파일을 덮어쓰거나 자동 승격하지 않는다.

**현재 완료 범위는 파일 reader/writer, Qwen 계열 MLX importer, 제한된 expert 슬롯 IO다. native Metal/CUDA/CPU 생성 경로와 GPU paging 연결은 아직 미구현이다.** CLI가 v2를 읽거나 검증하는 것을 모델 생성 성공으로 표시하지 않는다.

## 사용과 실모델 검증

```sh
scripts/loom model import /path/to/mlx-checkpoint --native --output /path/to/new-model.lmx --json
scripts/loom model inspect /path/to/new-model.lmx --verify --json
```

기존 import는 v1 호환 경로를 유지한다. native import는 명시한 새 파일에만 게시하며 현재 모델 목록에 실행 가능 모델로 자동 등록하지 않는다. inspect JSON은 `native:true`, `version:2`, `runtime_ready:false`를 반환한다.

Qwen3-Coder-30B-A3B-Instruct-3bit의 완전한 세 shard에서 변환하고 검증했다. 원본은 변경하지 않았다.

| 항목 | 측정 |
| --- | ---: |
| Native 파일 | 13,382,200,785 bytes |
| Index | 9,874,897 bytes |
| HF logical tensor / expert block | 18,867 / 6,144 |
| Import wall time / peak footprint, release | 118.96 s / 195,166,832 bytes |
| 전체 block hash 검증 / peak footprint, release | 25.54 s / 43,303,440 bytes |
| 한 expert block | 2,064,384 bytes |
| IO 검사 슬롯 풀, 16 experts | 33,030,144 bytes |
| 첫 top-8 읽기 | 16,515,072 bytes |
| 같은 top-8 재요청 | 추가 읽기 0 bytes |

슬롯 검사는 debug 빌드에서 metadata와 선택 expert만 읽었다. attention/router 등 resident weight를 실행용으로 로드하거나 GPU 연산을 수행한 검사가 아니다. plan의 resident payload 예산은 tokenizer를 포함해 685,980,606 bytes다. 이 수치를 슬롯 메모리나 전체 추론 footprint와 혼합하지 않는다. 합성된 ID 요청으로 cache hit를 검사했으므로 실제 routing hit rate나 생성 tok/s를 뜻하지 않는다.

실측 원문: [2026-10-09.json](../bench/lmx-native/2026-10-09.json). 검증 파일은 `~/.loom/artifacts/lmx-native/Qwen3-Coder-30B-A3B-Instruct-3bit-e3f38b2b.lmx`이며 base identity는 `e3f38b2b54da9f3cbf95edba46fc865a86eeebfdbaafc2bd5bc80488d73e6d90`이다.

## Binary header

정수는 little-endian이다. header는 128 bytes, 각 block과 index 시작점은 16,384 bytes 정렬이다. 4 KiB와 16 KiB 페이지에 모두 맞추는 파일 배치이며 runtime 장치 선택을 고정하지 않는다. 정렬 사이의 빈 공간은 weight 의미나 base identity에 포함하지 않는다.

| Byte offset | 길이 | 내용 |
| --- | ---: | --- |
| 0 | 8 | magic `LOOMLM2\0` |
| 8 | 4 | version = 2 |
| 12 | 4 | header length = 128 |
| 16 | 8 | JSON index의 파일 offset |
| 24 | 8 | JSON index bytes, 최대 32 MiB |
| 32 | 8 | 전체 파일 bytes |
| 40 | 32 | index의 raw SHA-256 |
| 72 | 56 | reserved, 전부 0 |

index는 정확히 EOF에서 끝난다. 알 수 없는 버전·field·enum, 중복 JSON key, 범위 overflow, 정렬 위반, block 중첩, 잘못된 shape/크기, 해석하지 못하는 recipe는 거부한다. 파일 open은 header와 index만 읽는다. 각 weight block의 SHA-256은 사용 시 검증한다. 전체 검증은 64 KiB scratch로 순차 수행한다.

writer는 목적지와 같은 디렉터리의 임시 파일에 기록하고 hash·범위·모델 의미를 검증한 뒤 원자적으로 게시한다. 기존 목적지는 교체하지 않는다. 중간 오류·취소·최종 callback의 취소는 미완성 목적지를 남기지 않는다. 실행은 read-only 파일과 디렉터리에서도 읽기만 수행한다.

## Index와 실행 의미

index 필드: `version`, `model`, `tensors`, `blocks`, `tokenizer`, `prompt`, `provenance`.

recipe 1의 모델 정의는 Qwen2/Qwen3/Qwen3 MoE, hidden/layer/vocab/context, attention head·KV head·head dimension, QKV bias, Q/K RMS norm, RMS epsilon, RoPE theta, FFN 크기, tied embeddings를 명시한다. causal attention, 전체 head의 split-half RoPE, SiLU(gate)×up를 사용한다. scaled/partial RoPE, sliding window, shared expert, sparse/dense 혼합 layer recipe는 아직 지원하지 않는다.

MoE는 expert 수, top-k 수와 선택 확률 재정규화 여부를 저장한다. router softmax 뒤 top-k를 선택한다. 일부 expert를 임의로 생략하거나 다른 가중치로 대체하지 않는다.

각 tensor는 HF 정식 이름, typed role, 정확한 row-major rank/shape, storage를 갖는다. recipe가 요구하는 모든 role과 shape를 검증하고 불필요한 tensor도 거부한다. tied embedding/head는 동일 storage descriptor를 공유하며 물리 데이터를 한 번만 저장한다.

## 가중치 표현

| Storage | 의미 |
| --- | --- |
| Dense | F32/F16/BF16, little-endian, 논리 원소 수와 정확히 일치하는 bytes |
| Affine revision 1 | unsigned packed code × scale + bias. bias가 없으면 0. bits=2/3/4/5/6/8, 양의 group size, 마지막 논리 축으로 grouping |

code는 **행마다 연속된 little-endian bitstream**이다. 열 `i`의 bit 위치는 `i × bits`이며 U32 경계를 넘을 수 있다. 행 끝은 byte 경계로 채운다. scale과 bias는 각각 F32/F16/BF16 dtype와 block 상대 범위를 갖는다. 그룹은 행 경계를 넘지 않는다. group 16도 파일에 표현할 수 있다. backend 지원 여부는 별도 판정이다.

현재 importer는 source MLX affine의 code·scale·bias bytes를 그대로 옮긴다. bundled MLX의 3/5/6bit packing이 이 수식·행 배치와 일치하는 것을 확인했다. dense F32를 F16으로 내리지 않는다. K-quant의 factored 표현은 아직 구현하지 않았다. GGUF native importer도 아직 없다.

## MoE block 배치와 슬롯 IO

block 종류는 `resident`, `expert(layer, expert)`, `tokenizer`다. expert 하나의 gate·up·down code와 scale/bias를 **한 개의 정렬 block**에 모은다. 서로 다른 expert를 같은 block에 섞지 않는다. router·attention·norm·embedding은 resident block으로 분리한다.

MLX의 `switch_mlp.PROJ` rank-3 bank `[expert, output, input]`는 importer에서 다음 HF 이름의 rank-2 tensor들로 나눈다. transpose나 역양자화는 하지 않는다.

```text
model.layers.L.mlp.experts.E.gate_proj.weight
model.layers.L.mlp.experts.E.up_proj.weight
model.layers.L.mlp.experts.E.down_proj.weight
```

token마다 `route → 필요한 expert ID의 합집합 → 슬롯 확보/읽기 → slot ID로 remap → 실행 → 완료 후 슬롯 재사용` 순서가 필요하다. `ExpertCache<B>`는 호출자가 소유한 고정 크기 버퍼에 positional read를 수행한다. hit는 IO가 없고 miss는 해당 expert block만 읽고 hash를 확인한다. LRU 교체 전에 요청 집합 전체를 보호해 뒤에서 쓸 hit를 앞선 miss가 내쫓지 못하게 한다. 부분 읽기나 hash 실패를 유효 expert로 게시하지 않는다.

`ExpertBatch`의 borrow는 요청 슬롯을 보호한다. GPU backend는 consuming dispatch가 끝날 때까지 lease를 유지해야 한다. **이 수명 규약을 Metal event/command completion에 연결하는 코드는 아직 없다.** 현재 cache 검사는 CPU caller buffer로 IO·교체·용량을 검증한다.

`ExpertPlan`은 resident payload(토크나이저 포함), caller가 지정한 KV·workspace/runtime reserve와 가장 큰 expert의 정렬 크기로 슬롯 수를 계산한다. 모든 expert를 FP16으로 펼치지 않는다. 최소 top-k개 슬롯을 확보하지 못하면 거부한다. 보수적 prefill 상한은 `slots / top_k`이고, 실제 요청 ID 합집합이 슬롯 수를 넘으면 batch를 나누도록 오류를 반환한다. 물리 footprint 측정이나 allocator overhead를 이 계산으로 대신하지 않는다.

참고: llama.cpp의 [SSD paging PoC](https://github.com/ggml-org/llama.cpp/discussions/23324)는 compact Metal slot pool, LRU, `pread`와 GPU 동기화를 사용한다. [GPU expert cache PR](https://github.com/ggml-org/llama.cpp/pull/29887)는 host expert 중 miss만 GPU로 전송한다. 두 경로는 disk paging과 host→GPU cache라는 차이가 있다. Loom은 저장·슬롯·동기화 원리를 참고하며 llama.cpp 모델 실행을 native runtime으로 사용하지 않는다.

## Tokenizer, template, identity

tokenizer recipe 1은 BPE 알고리즘과 self-contained tokenizers JSON asset이다. vocab/merges, normalizer, pre-tokenizer, decoder, added token이 같은 파일 안에 있다. BOS/EOS/EOG와 add-BOS 정책은 typed descriptor에 저장한다. independent tokenizer library는 사용할 수 있으나 원본 model/vocab loader는 호출하지 않는다. prompt recipe 1은 Jinja와 JSON/Python 호환 의미이며 template 문자열을 index에 저장한다. 임의 파일 loader나 host code 실행을 template에 제공하지 않는다.

base identity는 domain separator `loom-native-base-v1`, model, tensors, 정렬된 block 이름·종류·bytes·hash, tokenizer, prompt의 deterministic JSON에 대한 SHA-256이다. provenance와 block의 파일 offset은 제외한다. 파일 위치·출처 변경과 물리 block 순서 변경에도 같은 base다. **block의 semantic 이름과 내부 packing은 identity 계약의 일부다.** 이후 내부 packing을 바꾸는 규격은 identity schema 이행을 정의해야 한다. file index hash는 provenance와 물리 위치도 포함하는 별도 무결성 값이다.

LoRA는 base와 분리한 artifact로 유지한다. 이 단계는 base identity와 HF 이름을 제공하며 어댑터 파일·실제 적용은 다음 단계다.

## GGUF importer와 recipe 2 (2026-10-09 추가)

`scripts/loom model import <file.gguf> --native --output <new.lmx>`가 GGUF를 v2로 변환한다. 원본 parser는 import 때만 쓰고 v2 reader는 쓰지 않는다.

**가중치 저장**

| GGUF 타입 | v2 storage | 비고 |
| --- | --- | --- |
| F32/F16/BF16 | Dense, 원래 dtype | recipe 2의 일부 F32는 HF 값으로 변환(아래) |
| q4_0/1, q5_0/1, q8_0, q2_K–q6_K, iq1–iq4 전부, tq1_0/tq2_0, mxfp4 | **Ggml** (블록 바이트 그대로) | 재양자화·확장 없음. 네이티브 엔진이 ggml kernel로 그대로 실행 |
| 그 외(이름 없는 미래 타입) | Dense F16 (역양자화) | `precision_changed=true` |

2026-10-09 변경: 처음에는 k-quant를 MLX affine(group 32/16)으로 재포장했으나 scale을 F16으로 저장해 `d·sc`가 반올림되는 손실이 있었다. 지금은 모든 llama.cpp 블록을 원본 그대로 둔다(35B 파일 15.74 → 15.53 GB). MLX에서 GGUF를 돌릴 때는 `gguf_mlx`가 실행 시점에 변환한다.

**Recipe 2: Qwen3.5/3.6 MoE (`qwen35moe`)**
- `model.hybrid`에 선형 attention 층 목록, conv kernel, key/value head 수와 크기, partial RoPE(`rope_dims`, `rope_sections`), shared expert 크기를 둔다. recipe 1 파일에는 이 필드가 직렬화되지 않아 기존 identity가 바뀌지 않는다.
- 선형 층(Gated DeltaNet): `linear_attn.{in_proj_qkv,in_proj_z,in_proj_b,in_proj_a,conv1d,dt_bias,A_log,norm,out_proj}`.
- full attention 층: `q_proj` 출력이 head마다 `[query, gate]`이고 출력에 `sigmoid(gate)`를 곱한다. q/k norm 있음.
- 모든 층의 MoE에 shared expert(`mlp.shared_expert.*`)와 `sigmoid(shared_expert_gate·x)` 가중이 더해진다.
- 값은 HF 형태로 저장한다. GGUF 변환기가 바꾼 값을 되돌린다: zero-centered RMS norm(층 norm, final norm, q/k norm)은 `w = v − 1`, `A_log = ln(−A)`. 선형 attention의 출력 norm은 그대로다. 같은 모델을 HF/MLX에서 가져와도 같은 값·같은 이름이 된다.
- MTP(next-token prediction) 층은 생성에 쓰지 않아 제외한다.

**importer 수정**: MoE GGUF의 expert bank `[expert, rows, cols]`에서 expert마다 연속 byte 구간을 잘라 각 expert block에 쓴다(이전에는 bank 전체를 기록). 원본과 role의 원소 수·행 길이를 대조한다. vocab은 `token_embd` 행 수에서 읽는다. MoE의 FFN 크기는 `expert_feed_forward_length`를 쓴다.

**tokenizer**: `tokenizer.ggml.pre`에 맞는 Qwen 분할 정규식(qwen2, qwen35)과 NFC, ByteLevel을 HF tokenizer.json 형태로 합성한다. `token_type`이 control/user-defined인 토큰은 통째로 매칭한다. EOG는 EOS/EOT/EOM id와 turn을 끝내는 control 토큰(`<|im_end|>`, `<|endoftext|>` 등)이다. 알 수 없는 pre-tokenizer는 거부한다.

**실측 (Qwen3.8-35B-A3B IQ3_M, 16.34 GB GGUF, M4 Pro 24GB)**

| 항목 | 값 |
| --- | ---: |
| 변환 시간 | 70.6 s (release) |
| 결과 파일 | 15,743,231,740 bytes, 31,333 tensor, expert block 10,240 |
| 층 구성 | 40층 = 선형 30 + full attention 10 |
| 전체 block hash 검증 | 43.9 s, peak footprint 52 MB |
| tokenizer parity | llama.cpp vocab과 5개 샘플(코드, 한국어, 이모지, `<think>`, `<tool_call>`) 토큰 일치 |

재현: `LOOM_TOKENIZER_PARITY=<gguf>:<lmx> cargo test --release --lib tokenizer_matches_llama_cpp -- --ignored`.

## 네이티브 실행 엔진 (2026-10-09)

`native/lmx/engine.cpp`(C++, ggml) + `lmx_engine.rs`(FFI, 설정) + `lmx_native_runtime.rs`(채팅, 캐시). llama-cpp-sys-2가 빌드한 ggml을 그대로 링크하므로 Metal/CUDA/Vulkan/CPU 어느 backend든 같은 코드다(빌드는 `build.rs`의 `build_lmx_engine`, `cfg(loom_lmx_engine)`). 파일 IO는 POSIX `pread`/Windows `ReadFile` 분기.

**구조**
- 상주: expert를 뺀 가중치만 backend 버퍼에 올린다(35B: 1.19 GB). token embedding은 올리지 않고 현재 토큰 행만 파일에서 읽어 CPU에서 역양자화한다.
- expert: 층마다 router를 먼저 돌려 top-k id를 읽고, 없는 expert만 파일에서 slot pool로 읽은 뒤 expert FFN을 slot 위에서 `mul_mat_id`로 계산한다. pool은 layout(gate/up/down 타입·shape)별, LRU. Metal은 한 `mul_mat_id`가 1024 expert까지만 볼 수 있어 pool을 ≤1024 slot bank로 나누고 층을 bank에 나눠 배정한다.
- 그래프: 토큰 한 번에 층 수+1개. 그래프 l = [층 l−1 expert FFN + 층 l attention + 층 l router].
- **prefetch**: 그래프 l에서 층 l+1의 router를 층 l의 FFN 입력에 미리 적용해 다음 층 expert를 예측하고, GPU가 계산하는 동안 백그라운드 스레드가 읽어 둔다(slot은 미리 점유해 실행 중 그래프와 겹치지 않음). 예측 적중 82%.
- **KV 캐시 압축 (TurboQuant 방식)**: full-attention 층의 K/V를 `q8_0`(기본) 또는 `q4_0`로 저장. 저장 전에 head마다 직교 회전 M = Walsh-Hadamard·랜덤 부호/√d를 Q, K, V에 곱해 outlier 채널을 펴고(q·k 보존), attention 출력에 Mᵀ를 곱해 되돌린다. 하이브리드라 KV가 있는 층이 10/40뿐이고 recurrent state는 고정 크기다.
- **체크포인트·prefix 재사용**: recurrent state는 임의 위치로 자를 수 없으므로 요청마다 (a) 생성 프롬프트 직전(대화 끝)과 (b) 프롬프트 끝에서 recurrent state(conv + DeltaNet, 35B 약 63 MB)를 스냅샷한다. 다음 요청은 live 시퀀스 또는 공통 prefix 안의 가장 긴 체크포인트로 되돌아가 나머지만 계산한다. KV는 복사하지 않는다(체크포인트 아래 위치는 되감기 전까지 덮어쓰이지 않음). Qwen 템플릿이 이전 턴의 `<think>`를 지워도 (a)가 항상 prefix라 재사용된다.
- 채팅: GGUF에서 가져온 chat template, `native_runtime`의 도구 호출 파서/think 분리 재사용, temperature+top-p 샘플러, UTF-8 경계를 지키는 증분 detokenize. 종료 시 `atexit`로 엔진을 먼저 해제(ggml Metal 소멸자 abort 방지).
- 설치: `<models>/lmx-native/*.lmx`. 모델 목록에 `lmx` endpoint의 `native/<원본 상대경로>`, 표시 이름 `<이름> · 네이티브`.

**자동 설정 (사용자가 아무것도 정하지 않아도 됨)**
- expert 슬롯 예산: 모든 expert가 들어가면 전부. 아니면 RAM의 1/4(최소 4 GiB)을 OS·다른 프로그램 몫으로 남기고, 로드 시점의 가용 메모리(여유 1.5 GiB)와 GPU working set 여유로 제한.
- 메모리 압박 대응: 2초마다 가용 메모리를 보고 RAM의 5%(최소 1 GiB) 아래로 떨어지면 오래 안 쓴 슬롯의 페이지를 OS에 반납(`madvise`, 풀 재할당 없음). 여유가 다시 생기면 복구한다. 슬롯이 줄면 prefill chunk도 자동으로 줄인다. (Windows는 슬롯만 비활성화하고 페이지 반납은 아직 없음)
- context: KV 캐시에 RAM의 1/32(256 MiB–4 GiB)를 쓰는 범위에서 8K부터 2배씩, 모델 최대까지. 24 GB 머신 → 65,536.
- 다른 로컬 모델: 엔진이 모델을 올리면 idle 상태인 llama.cpp/MLX 모델을 내리고, 그쪽이 모델을 올리면 엔진을 내린다(사용 중이면 건드리지 않음).
- 슬롯 풀은 GPU가 매핑하는 page-aligned 호스트 메모리(`buffer_from_host_ptr`, Apple/통합 GPU). 파일에서 슬롯으로 직접 읽어 staging 복사가 없고 여러 스레드로 읽는다. 지원하지 않는 backend(CUDA 등)는 device 버퍼 + staging.
- 로드 직후 백그라운드 스레드 4개가 슬롯 풀을 파일 순서로 예열(35B 11 GB에 약 3.6초). 예열 중인 슬롯은 eviction 대상이 아니고, 요청이 쓰면 그 슬롯만 기다린다.

**수동 조정 (선택)**: `LOOM_LMX_EXPERT_MB`(지정하면 압박 대응도 끔), `LOOM_LMX_CONTEXT`, `LOOM_LMX_KV`(`q8_0`|`q4_0`|`f16`…), `LOOM_LMX_KV_ROTATE=0`, `LOOM_LMX_PREFETCH`(기본 8, 0=끔), `LOOM_LMX_CHUNK`(기본 1024), `LOOM_LMX_THREADS`, `LOOM_LMX_GPU=0`, `LOOM_LMX_DEVICE_SLOTS=1`(매핑 대신 device 버퍼).

**검증 (Qwen3.8-35B-A3B IQ3_M, M4 Pro 24 GB, Metal)**

| 항목 | 결과 |
| --- | --- |
| llama.cpp(같은 GGUF) 대비 greedy | 48토큰 완전 일치. KV f16 / q8_0+회전 / q4_0 / q4_0+회전 모두 |
| 첫 logit 최대 오차 | f16 0.90, q8_0+회전 0.88, q4_0 1.40, q4_0+회전 1.18 (llama.cpp 쪽은 CPU) |
| decode, 예산 2 GB (고정, 초기 구현) | 10.3 tok/s, RSS 3.8 GB |
| 예산별 (프롬프트 1024, 직접 읽기 전) | 2 GB 10.3 / 4 GB 11.0 / 6 GB 12.7 / 8 GB 14.1 / 11 GB 14.9 tok/s |
| **자동 설정 (현재)** | 예산 10.3 GB, context 65,536 → prefill(2048) **259 tok/s**(예열 후 322), decode **27.7 tok/s**, hit 99%, RSS 12.3 GB |
| 메모리 압박 시뮬레이션 | 4 GB 반납 후에도 같은 답, 여유 회복 시 원복 |
| 2턴 대화 | 두 번째 요청 60토큰 중 32토큰 캐시 재사용, 0.8 s |

비교: llama.cpp로 같은 GGUF를 GPU에 올리면 약 19 tok/s지만 12 GB 이상 상주한다.

재현: `LOOM_LMX_ENGINE_PARITY=<gguf>:<lmx>` → `engine_matches_llama_cpp`, `LOOM_LMX_BENCH=<lmx>` → `engine_bench`, `LOOM_LMX_NATIVE_CHAT=1` → `native_chat_reuses_prompt_prefix` (모두 `cargo test --release --lib <이름> -- --ignored --nocapture`).

**첫 응답까지의 시간 (2026-10-09 추가)**
- 디스크 prefix 캐시: 시스템 프롬프트+도구 정의까지(모든 세션이 공유)의 상태(KV + recurrent, 2k 토큰에 87 MB)를 `<models>/lmx-native/.prefix/<identity>/`에 저장하고, 새 세션 첫 요청에서 복원(0.04 s). 키는 모델·KV 설정·정확한 토큰. 모델당 최근 8개 유지.
- 사전 로드: TUI에서 네이티브 모델이 선택되면(시작 포함) 엔진 로드와 expert 예열을 백그라운드로 시작한다(`lmx_native_runtime::preload`).
- 예열 새치기: 요청이 아직 예열되지 않은 슬롯을 쓰면 큐를 기다리지 않고 그 자리에서 읽는다(원자적 claim).
- 엔진의 파일 읽기는 OS 페이지 캐시를 거치지 않는다(macOS `F_NOCACHE`, Linux `fadvise`): 슬롯과 캐시에 같은 데이터가 두 벌 생기지 않게.
- 예산은 RAM 총량 기준(로드 순간의 여유가 아님)으로 잡고, 예열은 여유 메모리가 (RAM 5% + 1 GiB) 아래로 내려가면 멈춘다.
- 실측(2k 토큰 에이전트 프롬프트, 새 프로세스): 첫 토큰 9.2 s → 2.4 s(로드 1.2 s 포함). 사전 로드되면 로드 시간도 사라진다.
- whole-pass decode: expert가 모두 상주하고 재시작이 드물 때만 토큰 전체를 그래프 하나로 실행(GPU slot 매핑 테이블, CPU 검증, miss 층 FFN부터 재개, recurrent state double buffer). 측정상 이득은 작아(전체 상주 시 28 vs 28 tok/s) 적응형으로만 켠다.
- 비교(같은 GGUF, llama.cpp Metal, 메모리 맞춤): prefill 880 tok/s, decode 35 tok/s, 로드 15 s, 모델 파일 전체를 페이지 캐시로 상주. 엔진은 전체 expert가 상주할 때 prefill 585 / decode 34이고, 이 24 GB 머신에서는 메모리 여유 때문에 약 11 GB만 채워 prefill 300대 / decode 28–30.
- LMX v1(GGUF 포장)은 이제 기본으로 GPU(Metal, 맞춤 분할)를 쓴다. CPU 강제는 `--engine cpu`일 때만.

**모델 호출 전 준비 단계 (Loom Core, 9.5k 파일 저장소)**

| 단계 | 이전 | 지금 |
| --- | --- | --- |
| 작업 공간 준비(overlay) | 2.3 s/턴 | 1.0 s (파일 clone 병렬화, 상위 디렉터리 검사 1회) |
| 코드 인덱스 | 2.1 s/턴(TUI가 턴마다 Core를 새로 열어 캐시가 버려짐) | 첫 턴 2.1 s, 이후 0.03 s (저장소별 hub 공유 + 같은 revision snapshot 재사용) |
| overlay 정리 | 턴 끝에 0.6 s 동기 대기 | 백그라운드(`LOOM_OVERLAY_SYNC_CLEANUP=1`로 동기) |

백그라운드 정리 중 fork된 git이 잠깐 파일 잠금을 물고 있을 수 있어 작업 잠금은 최대 200 ms 재시도한다.

**남은 일 (우선순위 순)**
1. prefill 계산 자체(2k 토큰 3.1 s vs llama.cpp 2.3 s 전체): 커널 선택·FFN 합산 방식 점검.
2. 매핑 불가 backend(CUDA)에서 병렬 staging 로드.
3. affine storage(MLX에서 가져온 v2) 실행: 자체 Metal/CUDA kernel 필요.
4. recipe 1(dense Llama/Qwen) 엔진 지원.
5. Linux/Windows CPU·CUDA 빌드 실측(코드는 backend 무관하게 작성, 이 머신에서는 Metal만 검증).
6. GGUF에서 온 qwen35 선형 attention 텐서는 llama.cpp 변환기가 V head를 재배열한 순서(tiled)다. HF safetensors 순서(interleaved)와 다르므로 HF에서 가져온 같은 모델과 identity가 다르다. LoRA 정합을 위해 importer에서 되돌리고 엔진에서 interleaved 브로드캐스트를 쓰는 것이 맞다.

## 검증과 다음 순서

회귀 검사는 native file roundtrip, header/index 변조·truncation·중복 key·overlap 거부, 취소와 no-overwrite 게시, read-only 독립 파일 읽기, exact rank와 tied sharing, odd-bit expert의 component bytes 보존, tokenizer encode/decode, 슬롯 hit/miss/합집합 보호·cold expert checksum을 다룬다.

다음은 native descriptor를 자체 Metal 실행기에 연결하고 BPE/template을 파일 내부 데이터에서만 실행하는 것이다. 그 뒤 route와 `ExpertCache`를 GPU completion 수명에 연결해 실제 30B의 첫 토큰·prefill·decode·peak footprint·읽은 bytes·hit rate를 측정한다. 생성 parity와 메모리 예산을 통과하기 전 v1 또는 기존 MLX 경로를 v2 실행이라고 부르지 않는다. 장치 선택은 Metal → CUDA → CPU이며 구현하지 않은 native backend는 정확히 거부한다.
