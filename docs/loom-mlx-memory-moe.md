# MLX: 메모리 우선 추론과 모델 호환성

상태: 2026-10-08. 우선순위는 메모리, 그다음 속도이며, MoE와 다양한 모델 계열 지원이 필수다. 아래에서 구현·검증한 범위와 후속 목표를 구분한다.

다중 양자화, 플랫폼 공통 구조, 추론과 도구의 동시 실행을 포함한 후속 구조는 [로컬 엔진 재설계](loom-local-engine-redesign.md)를 기준으로 한다. 이 문서는 기존 MLX 변경과 실측 기록을 유지한다.

2026-10-09 추가: 자체 MLX·일반 명령·Unix PTY의 공통 자원 예약과 제한된 prefill pass를 연결했다. [단계 A 구현 기록](loom-local-resource-admission.md)에 실제 동작·설정·제한·검증 결과를 기록한다.

2026-10-09 native LMX 진행: [v2 규격과 expert 슬롯 IO](loom-lmx-v2-native.md)를 구현했다. expert별 gate/up/down과 양자화 계수를 독립 block에 모으는 importer와 제한된 caller-owned 슬롯 읽기를 제공한다. 현재 MLX 생성 경로의 전체 bank 로딩은 아직 교체하지 않았으며 GPU paging은 연결 대기다. 설치된 30B의 세 shard는 현재 선언된 길이와 모두 일치한다. 아래 불완전 다운로드 기록은 이전 시점의 측정이다.

## 이번에 적용한 내용

- 기본 prefill을 512에서 128토큰으로 줄였다. `LOOM_MLX_PREFILL`로 조정할 수 있다.
- MLX 재사용 버퍼 한도를 256MiB에서 64MiB로 줄이고 가중치 로딩 전에 적용한다. `LOOM_MLX_CACHE_MB=0`이면 재사용 버퍼를 끌 수 있다. **이 한도는 전체 프로세스 메모리 상한이 아니다.**
- 새 대화로 KV 위치가 0으로 돌아가면 이전 대화의 KV 버퍼를 해제한다. 접두사 재사용과 speculative rollback은 보존한다.
- QKV 융합은 `LOOM_MLX_FUSE_QKV=1`로 명시적으로 켠다. 기본값은 꺼짐이다. 켰을 때는 레이어별로 압축 가중치를 합치고 원본 참조를 해제해 중복 생존 기간을 줄인다.
- Qwen3 MoE의 top-k router, 선택 expert의 SwiGLU, 선택 확률 정규화, 가중 합산을 추가했다. `gather_qmm`은 압축된 expert bank에서 선택한 expert만 계산한다. 전체 expert를 FP16으로 역양자화하지 않는다.
- tensor마다 압축 폭을 확인해 3/4bit expert와 8bit router 같은 혼합 비트 정밀도를 처리한다. group size는 모델 내에서 같아야 하며, affine 양자화만 지원한다.
- 로딩 도중 오류가 나도 이미 생성한 모델과 레이어가 해제되도록 소유권을 수정했다.
- 벤치마크 JSON에 MLX 활성·피크 활성·재사용 버퍼 크기를 추가했다. 물리 메모리 피크와 MLX allocator 피크를 혼합하지 않는다. 물리 피크는 프로세스 생애의 최대값이므로, 비교 측정은 매번 새 프로세스로 실행한다.

관련 구현은 `loom-core/native/mlx/engine.cpp`, `loom-core/src/models/mlx_runtime.rs`, `loom-core/src/models/native_runtime.rs`다.

## 실측과 검증

Apple M4 Pro, 24GiB, Qwen3-4B 4bit. 프롬프트는 기존 bench의 반복 코드 입력이며 생성은 greedy다. 수치는 GB(10⁹ bytes)다.

프롬프트 512, 생성 64, prefill 512로 고정하고 각 설정을 3번씩 순서를 바꿔 실행했다.

| 설정 | 생성 tok/s 중앙값 | 물리 피크 중앙값 | 종료 시 MLX 재사용 버퍼 대략 |
| --- | ---: | ---: | ---: |
| 이전 버퍼 한도 256MiB, 융합 끔 | 33.1 | 3.204GB | 268MB |
| 버퍼 한도 64MiB, 융합 끔 | 31.4 | 3.188GB | 66MB |
| 버퍼 한도 64MiB, QKV 융합 | 34.5 | 3.346GB | 57MB |

9개 실행의 생성 토큰은 전부 일치했다. QKV 융합은 물리 피크를 약 150MB 늘렸다. 버퍼 한도 감소는 종료 시 보유 버퍼를 약 200MB 줄였지만 이 입력의 피크 감소는 약 15MB였다. 측정 중 시스템 부하가 있었고 속도 분산이 커서 속도 우위를 확정하지 않는다.

프롬프트 2,048, 생성 64, 버퍼 한도 64MiB, 융합 끔으로 각각 한 번 더 측정했다.

| KV | prefill 묶음 | 물리 피크 | MLX 피크 활성 | F16/512 대비 생성 토큰 일치 |
| --- | ---: | ---: | ---: | ---: |
| F16 | 512 | 3.455GB | 3.256GB | 64/64 |
| F16 | 128 | 3.247GB | 2.998GB | 64/64 |
| Q4 | 128 | 3.063GB | 2.864GB | 63/64 |
| Q8 | 128 | 3.124GB | 2.945GB | 64/64 |

128토큰 묶음은 이 측정에서 물리 피크를 약 208MB 줄였다. Q4와 Q8은 KV 메모리를 더 줄이지만, 토큰 일치는 작은 입력에서의 관찰일 뿐 품질 동등성 검증이 아니다. 따라서 KV 정밀도 기본값은 F16을 유지하며, 사용자가 `LOOM_NATIVE_KV=q4_0` 또는 `q8_0`로 선택한다. 양자화 KV attention은 현재 별도 행렬곱과 softmax이며 fused attention이 아니다.

원본 결과:

- [버퍼/QKV 비교](../bench/mlx-memory-moe/dense-comparison.json)
- [입력 묶음/KV 비교](../bench/mlx-memory-moe/context-comparison.json)

검증 범위:

- 독립적인 expert별 기준 계산과 native MoE를 비교한 72개 경우: FP32/BF16, 비양자화/3bit/4bit expert, 8bit router, prefill/decode 형태, top-k 1/2/3, 확률 정규화 켬/끔.
- KV 접두사 rollback 보존과 새 대화에서의 버퍼 해제.
- 작은 MoE safetensors를 저장·로딩한 뒤 prefill, decode, verify, 캐시 재사용을 비교하는 native 회귀 검사.
- Rust 모델 탐색 및 지원하지 않는 변형 거부 검사.
- 설치된 dense 4B 모델의 실제 스트리밍 결과와 최종 응답 일치 검사.

설치된 Qwen3-Coder-30B-A3B-Instruct-3bit의 첫 shard가 불완전하다. 실제 2,456,331,462 bytes, 선언된 크기는 5,324,399,729 bytes이며 2,868,068,267 bytes가 부족하다. 나머지 두 shard의 크기는 선언과 일치한다. 이어받기는 연결 시간 초과/DNS 오류로 실패했다. **실제 30B 모델의 생성 품질·속도·메모리는 아직 검증하지 못했다.** 완전한 세 shard의 합계는 13,358,291,526 bytes다.

MoE의 활성 파라미터 수는 계산량을 줄인다. 이 구현은 전체 압축 expert bank를 로딩하므로 활성 3B만큼의 메모리로 30B가 실행된다는 의미가 아니다. 디스크에서 expert를 교체하는 기능은 아직 없다.

## 현재 지원 경계

| 경로 | 범위 | 제한 |
| --- | --- | --- |
| 자체 MLX dense | Qwen2, Qwen3, Llama, Mistral 계열의 구현과 일치하는 변형 | SiLU, 일반 RoPE, 일반 causal attention. scaled RoPE·sliding window는 미지원 |
| 자체 MLX MoE | Qwen3 MoE, MLX `switch_mlp` expert bank | affine, 동일 group size, shared expert 없음. 실제 30B 체크포인트 검증 대기 |
| 기존 llama.cpp | 해당 번들 버전이 구현한 GGUF 아키텍처 | MLX 체크포인트가 자동 변환되거나 자동 대체되는 것은 아님 |
| 기존 CPU/CUDA 경로 | `Backend::Cpu`, CUDA feature가 포함된 `Backend::Cuda` | Windows/Linux 실기기 검증은 이번 작업에서 하지 않음 |

다른 활성화, scaled RoPE, sliding window, shared expert, 지원하지 않는 양자화 모드는 현재 자체 MLX 경로에서 거부한다. 모델 이름이 같다는 이유로 다른 연산을 흉내 내어 실행하지 않는다. Qwen3.5/hybrid/GDN, DeepSeek 계열, GPT-OSS, Gemma 계열, 멀티모달까지 자체 MLX가 모두 지원한다고 주장할 수 없다.

## 추가 최적화: 활성화 융합

MLX-LM의 [컴파일된 SwiGLU](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/models/activations.py)를 참고해 dense와 MoE의 `gate * sigmoid(gate) * up`을 shapeless compiled graph로 묶었다. 중간 활성화 버퍼를 줄이며 가중치를 복제하지 않는다. 기본 활성화이며 `LOOM_MLX_COMPILE_ACTIVATIONS=0`으로 기존 경로를 선택한다. 최초 컴파일 비용은 cold load에 포함된다.

4B 4bit, F16 KV, 프롬프트 512, 생성 128, prefill 128, 버퍼 한도 64MiB, QKV 융합 끔으로 ON/OFF 각 3회 교차 측정했다.

| 활성화 융합 | 생성 tok/s 중앙값 | 물리 피크 중앙값 |
| --- | ---: | ---: |
| 끔 | 82.10 | 2.907GB |
| 켬 | 82.42 | 2.884GB |

생성 128토큰은 6회 모두 같았다. 물리 피크는 약 23.4MB 감소했다. 생성 속도 차이 약 0.4%는 측정 분산보다 작아 속도 개선으로 단정하지 않는다. 이전 표와 시스템 상태·생성 길이가 다르므로 이전 절대 속도와 직접 비교하지 않는다. 원본은 [활성화 비교](../bench/mlx-memory-moe/activation-comparison.json)다. Native MoE 72개 경우와 작은 체크포인트의 prefill/decode/verify 검사는 융합 ON/OFF 모두 통과했다.

다음 후보는 압축 KV에서 attention score 전체를 저장하지 않는 fused attention, GPU token을 CPU로 매번 가져오기 전 다음 계산을 시작하는 제한된 비동기 pipeline, prefill의 MoE expert별 작업 묶기다. 현재 KV 갱신이 전체 복사를 유발하는지는 profiler로 확인해야 하며, `slice_update` 호출만 보고 복사를 단정하지 않는다. 별도 draft 모델은 추가 가중치가 필요하므로 메모리 우선 정책에서는 기존 prompt lookup의 적중률·검증 비용에 따른 조절부터 검토한다.

## 다운로드 뒤 실행용 변환

사용자가 제안한 다운로드 후 변환을 다음 import 경계로 둔다. **현재 다운로드 코드는 GGUF 중심이며 아래 자동 변환기는 아직 구현하지 않았다.**

`다운로드 → 파일 무결성 확인 → 구조·양자화 판별 → 실행 장치에 맞는 계획 → 제한된 메모리로 변환 → 검증 → 실행용 사본 게시`

- 장치별 실행 사본: Apple은 MLX가 직접 읽는 압축 safetensors, 기존 Windows/Linux 경로는 backend가 지원하는 GGUF를 우선한다. 이미 호환되는 모델은 그대로 재사용한다. 공통 manifest로 관리하되 하나의 새 바이너리 포맷을 먼저 만들 필요는 없다.
- 다운로드 전 저장소 메타데이터로 이미 적합한 MLX/GGUF 변형이 있는지 먼저 확인한다. 적합한 사본이 있으면 불필요한 원본 다운로드와 재양자화를 줄일 수 있다. 변환이 필요한 경우에만 import 단계를 실행한다.
- 첫 변환은 정밀도를 보존하는 재배치다. 지원하는 계열의 Q/K/V를 미리 합친 압축 텐서, MoE expert bank, 정렬과 shard 구성을 준비한다. 이렇게 하면 런타임 QKV concatenate의 복사·일시 중복을 없앨 수 있다. 실제 메모리·속도 이득은 실행용 사본으로 다시 측정해야 한다.
- 혼합 양자화는 별도 선택이다. 원본 정밀도가 있는 경우에만 모델별 품질 평가를 거쳐 정밀도를 선택한다. 기존 GGUF 양자화를 MLX affine 등 다른 형식으로 재양자화하면 추가 오차가 생길 수 있다. 같은 비트 수가 같은 표현·품질을 의미하지 않는다.
- 변환 메모리는 가장 큰 텐서 전체가 아니라 처리 블록과 출력 버퍼 크기로 제한하는 것이 목표다. 전체 모델의 FP16 확장은 피한다. GGUF 양자화별 decoder와 expert block slicing 지원이 각각 필요하다.
- 원본 hash/revision, architecture, tokenizer/chat template, 원본·출력 양자화, layout, converter 버전, 필요한 backend 기능을 manifest에 기록한다. OS/GPU와 런타임 버전에 종속적인 컴파일 캐시는 가중치 사본과 따로 관리한다.
- 불완전한 다운로드는 변환하지 않는다. 임시 위치에서 작업하고, 재배치 시 값 보존 또는 재양자화 시 logits/품질 허용 기준을 검증한 다음 게시한다. 원본은 검증 전에 삭제하지 않는다. 같은 파일 hash와 변환 계획이면 실행용 사본을 재사용한다.
- 모델 구조의 지원 여부는 변환 전에 검사한다. 미지원 hybrid/GDN이나 router 변형을 텐서 이름 변경만으로 지원했다고 취급하지 않는다. 지원되는 기존 backend를 선택하거나 구체적인 미지원 이유를 반환한다.

MLX-LM의 [변환기](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/convert.py)는 MLX 모델 변환과 혼합 비트 recipe의 참고 구현이다. Loom의 목표인 블록 단위 메모리 상한과 GGUF 변환을 그대로 보장하는 것으로 간주하지 않는다.

## 대부분의 모델을 지원하기 위한 확장 순서

1. **메모리 계획을 공통 계층으로 만든다.** 가중치, KV, prefill 임시 버퍼, expert 작업 공간, OS 여유분을 각각 계산하고 입력 크기별 계획을 선택한다. 정확한 상한 거부와 adaptive chunking은 후속 구현이다. MLX 재사용 버퍼 한도만으로 전체 OOM을 막았다고 판단하지 않는다.
2. **체크포인트 형식과 실행 backend를 분리한다.** 모델의 architecture·attention·position encoding·quantization·expert layout으로 실행 가능 여부를 판단한다. 지원 여부는 모델 계열 이름 하나로 결정하지 않는다. 현재 provider 인터페이스와 llama.cpp 경로를 재사용한다.
3. **MLX-LM의 모델별 구현을 기준으로 확장한다.** scaled RoPE, sliding/local attention, shared expert와 router 변형, hybrid recurrent state를 독립 기능으로 추가한다. 모델별로 logits 오차, greedy 출력, 여러 단계의 cache rollback, 긴 입력 메모리 피크를 비교한다. Qwen3 MoE 하나를 지원한 것으로 모든 MoE가 지원되지는 않는다.
4. **GGUF→MLX는 스트리밍 변환으로 구현한다.** 텐서나 양자화 블록 단위로 변환하고 디스크에 캐시해 전체 모델을 FP16으로 동시에 확장하지 않는다. GGUF와 MLX의 양자화 layout이 다른 경우 재양자화 오차도 검증한다. 미구현 architecture나 양자화는 기존 GGUF 엔진에 남긴다. 현재 MLX의 GGUF reader만 호출하는 것은 이 목표를 충족하지 못한다.
5. **expert 메모리는 별도 정책으로 확장한다.** 압축 bank 상주를 기준으로 먼저 실측한다. 메모리가 부족할 때만 expert 캐시·지연 로딩을 검토한다. 선택 expert가 토큰·레이어별로 달라 SSD 접근과 page fault가 속도를 크게 낮출 수 있다. Apple 통합 메모리에서는 CPU 이동만으로 물리 RAM 사용량이 줄지 않는다.
6. **Windows/Linux는 기존 llama.cpp를 기준 구현으로 유지한다.** CPU/CUDA를 실기기에서 검증하고, 필요한 경우 Vulkan 등 backend를 추가한다. 자체 커널은 메모리·정확도·속도 비교에서 이득이 확인된 부분부터 대체한다. 호환성을 유지하면서 자체 엔진의 영역을 넓히는 방식이다.

위 항목은 후속 설계이며, 자동 backend 선택·GGUF 변환·expert 교체 기능은 아직 구현되지 않았다.

## 재현

저장소 루트에서:

```sh
cargo build --release --manifest-path custom-tui/Cargo.toml
python3 scripts/bench-mlx-memory --prefill 512 --repeats 3 --output /tmp/loom-mlx-memory.json

LOOM_MLX_PREFILL=128 custom-tui/target/release/loom model bench mlx/mlx-community/Qwen3-4B-4bit --kv f16 --prompt 2048 --gen 64 --json

c++ -std=c++20 -O2 -I loom-core/vendor/mlx/include loom-core/native/mlx/tests.cpp -L loom-core/vendor/mlx/lib -lmlx -Wl,-rpath,"$PWD/loom-core/vendor/mlx/lib" -o /tmp/loom-mlx-native-tests
/tmp/loom-mlx-native-tests
cargo test --release --manifest-path loom-core/Cargo.toml --lib -- --test-threads=1
LOOM_MLX_SMOKE=mlx-community/Qwen3-4B-4bit cargo test --release --manifest-path loom-core/Cargo.toml --lib mlx_stream_matches_response -- --ignored --test-threads=1
```

이미 실행 중인 Loom은 기존 바이너리를 사용하므로 새 기본값을 사용하려면 다시 시작해야 한다.

## 참고 구현

- [MLX-LM Qwen3 MoE](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/models/qwen3_moe.py): router와 expert 가중 합산의 기준.
- [MLX-LM switch layers](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/models/switch_layers.py): 압축 expert의 gather matmul. 메모리 복사를 수반하는 expert별 역양자화를 피하는 방향을 참고했다.
- [llama.cpp](https://github.com/ggml-org/llama.cpp): 여러 아키텍처와 CPU/Metal/CUDA 등 backend의 호환성 기준. upstream 최신 지원 목록과 Loom에 고정된 의존성 버전의 지원 목록은 다를 수 있다.
- 참고한 MLX-LM의 [MIT 고지](../licenses/mlx-lm-MIT.txt)를 보존했다.
