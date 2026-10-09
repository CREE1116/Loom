# 로컬 추론·도구 자원 예약 — 단계 A 구현 기록

2026-10-09. [재설계](loom-local-engine-redesign.md)의 첫 구현이다. 현재 연결 범위는 **자체 MLX + 일반 도구 명령 Runner + Unix PTY**다. GGUF 추론의 메모리 예약, 다중 세션 executor, 새 양자화 포맷, 자동 GGUF 변환은 후속 단계다.

## 실제 동작

- `base::resources`가 프로세스 안의 공통 메모리 예산과 RAII 예약을 소유한다. 모델과 effects 사이의 의존성을 추가하지 않았다.
- 가중치·KV 등 지속 메모리, 추론 임시 메모리, 도구 예상 메모리를 분리해 계산한다. MLX는 pass 완료 후 allocator 관측으로 지속 예약을 보정한다.
- 모델 로드에 필요한 가중치와 임시 공간은 함께 입장 심사한다. 계산이 끝나면 scratch 예약을 지속 예약으로 원자적으로 전환한다.
- 일반 명령은 spawn 직전 예약하고 프로세스 정리 뒤 반환한다. 오류 경로에도 프로세스를 정리하는 guard를 사용한다.
- PTY는 첫 응답을 반환한 뒤에도 supervisor가 예약을 소유한다. 프로세스 tree 종료·reap 뒤 예약을 반환하고 종료 상태를 게시한다.
- MLX 입력을 Rust에서 제한된 묶음으로 나눠 C++의 sampling 없는 prefill pass에 전달한다. 묶음 사이에서 예약을 반환하고 취소·deadline을 확인한다. 마지막 묶음에서만 다음 토큰을 sampling한다.
- 예산이 부족하면 prefill 묶음을 줄인다. 최소 묶음도 일시적인 도구 예약과 함께 들어가지 못하면 대기하고, 예약 반환 통지로 재개한다.
- 도구가 대기하거나 실행 중이면 MLX 재사용 버퍼를 비운다. 사용 중인 가중치와 KV는 유지한다. 문맥이나 정밀도를 몰래 변경하지 않는다.
- 도구 입장에 메모리가 부족하면 유휴 모델의 재사용 버퍼부터 회수한다. 이것으로 충분하면 prefix KV를 보존하고, 부족할 때만 유휴 KV를 해제한다. 가중치는 유지하고 다음 요청에서 원래 입력으로 KV를 다시 계산한다.
- 활성 요청은 KV를 회수 가능량에서 제외한다. 회수 callback은 모델 잠금을 기다리지 않고 확인하며, GPU pass나 요청이 소유한 상태를 해제하지 않는다.
- 취소·timeout·오류로 끝난 요청은 자신의 KV와 재사용 버퍼를 정리한다. 다른 활성 요청의 모델 잠금을 기다리다 종료한 요청은 그 상태를 건드리지 않는다. 마지막 생성 토큰에서의 취소도 최종 응답 전에 확인한다.
- 이미 resident인 가중치·상태와 최소 작업량만으로 예산을 넘으면 무한 대기 대신 구체적인 예산 오류를 반환한다.
- 모델 잠금 대기에도 취소·deadline을 적용한다. 현재 전체 요청의 mutex는 유지되며, 세션 간 실행 단위 스케줄링은 아직 없다.
- 자원 입장 오류에는 별도 machine code를 붙인다. 메모리 부족이나 자원 대기 timeout 때문에 router가 모델을 cooldown하거나 도구 미지원으로 표시하지 않는다.

## 기본 예산과 조정

| 설정 | 기본값 | 의미 |
| --- | --- | --- |
| `LOOM_LOCAL_MEMORY_MB` | 물리 RAM의 75% | 이 Loom 프로세스가 추적하는 예약의 총 예산 |
| `LOOM_TOOL_MEMORY_MB` | 1,024MiB | 실행 중인 명령 하나당 예상 예약 |
| `LOOM_MLX_PREFILL` | 128토큰 | 입력 처리 묶음의 상한. 메모리가 부족하면 더 작게 처리 |
| `LOOM_MLX_CACHE_MB` | 64MiB | MLX 재사용 버퍼 상한. 도구 압박 중에는 비움 |

MB 환경변수의 단위는 MiB다. 양의 정수만 유효하며 잘못된 값·0은 기본값을 사용한다. 단, 기존 `LOOM_MLX_CACHE_MB=0`은 버퍼 비활성화 값이다. 공통 예산과 도구 예약 설정은 최초 사용 시 읽으므로 변경하려면 프로세스를 다시 시작한다. 물리 RAM을 알아낼 수 없으면 공통 예산은 4GiB다.

임베딩 호출자는 `ResourceGovernor::new`와 `Runner::with_resources` / `TerminalManager::with_resources`로 명시적인 공통 예산을 전달할 수 있다. 일반 생성자는 동일한 process-local 자원 관리자를 사용한다.

## 관측과 제한

`ResourceGovernor::snapshot()` 및 MLX `model bench --json`의 `local_resources`에 다음을 기록한다.

- 지속 메모리·추론 scratch·도구 예약, 예약 피크.
- 대기 중인 추론/도구 수, 대기 횟수, 종료한 대기의 누적 시간(취소·timeout 포함).
- 관측된 MLX 메모리가 예상 예약을 초과한 횟수와 예산 초과 관측 횟수.
- 유휴 상태 중 회수 가능한 예약, 회수 횟수와 반환된 예약 bytes. 실제 allocator/물리 메모리 감소량과 구분한다.

**예약 통계는 실제 전체 물리 메모리 측정이 아니다.** 벤치의 프로세스 물리 피크와 MLX allocator 통계는 별도로 유지한다. 예약 peak와 counter는 프로세스 내 누적값이다.

현재 모델 예상치는 파일 크기 + 10% 여유 + CPU 사용량 128MiB, KV block·activation·attention score·logits·성장 시 중복 버퍼의 보수적 계산을 사용한다. 모델 로드 scratch는 256MiB로 예약하고, 실행 뒤 관측값이 이를 넘으면 초과를 드러낸다. 추정만으로 native allocator의 모든 할당을 차단하는 hard limit을 구현한 것은 아니다. 도구 예상치는 명령별 실측 학습 없이 위의 고정값을 사용한다.

이 단계는 외부 프로세스의 실제 RSS/tree peak·OS 메모리 압박·swap을 감시하거나 강제 제한하지 않는다. 다른 앱이나 다른 Loom 창의 사용량과 자동 조율하지 않는다. Linux 컨테이너의 메모리 제한도 아직 별도 감지하지 않는다. 도구의 CPU 병렬도, GPU/대역폭 throttling, 다중 세션별 cache 관리와 디스크 상태 저장도 후속 작업이다. 현재 유휴 KV 회수는 하나의 로딩된 MLX 모델에 적용한다.

대기 도구를 새 추론보다 우선한다. 지속적인 도구 유입에 대한 aging은 아직 없다. 이미 제출한 GPU kernel을 즉시 중단하지 않으며, synchronous pass 완료 사이에서 취소한다. 고정밀 SDPA scratch도 현재 보수적으로 score 공간을 계산하므로 긴 문맥에서는 필요한 것보다 작은 prefill 묶음이 선택될 수 있다.

회수 후에도 가중치 등 필수 메모리와 함께 들어가지 못하는 요청은 cache를 버리기 전에 거부한다. 활성 KV와 공존할 수 없는 도구도 명확한 예산 오류로 끝내므로, 도구가 그 KV의 반환을 기다리면서 소유한 추론까지 막는 교착을 만들지 않는다.

## 유휴 메모리 회수 실측

M4 Pro, Qwen3-4B-4bit, F16 KV, 입력 983토큰·출력 최대 8토큰, 재사용 버퍼 상한 64MiB. 도구 예약을 위해 메모리가 부족한 두 경우를 각각 검사했다.

| 회수 경로 | 반환한 예약 | MLX 활성 메모리 감소 | 프로세스 사용 메모리 감소 | 다음 요청 |
| --- | ---: | ---: | ---: | --- |
| 재사용 버퍼만 | 67.1MB | 0.3MB | 53.5MB | 기존 prefix 재사용 |
| 재사용 버퍼 + 유휴 KV | 218.1MB | 151.3MB | 198.1MB | KV 전체 재계산 |

수치는 MB(10⁶ bytes)다. 회수 전후 같은 프로세스의 현재 메모리를 비교한 결과이며, 전체 작업의 peak 감소량이나 일반적인 속도 개선율이 아니다. 재계산하면 KV 메모리는 다시 필요해진다. 압박이 없으면 callback을 호출하지 않고 prefix를 유지한다.

두 경우 모두 실제 Runner 도구가 실행됐고, 모델 handle은 유지됐다. 도구 종료 뒤 greedy 결과가 기준과 같았으며 cache-only 경로는 cached input을 재사용하고 KV 회수 경로는 cached input 0으로 재계산했다. [idle-reclaim.json](../bench/mlx-memory-moe/idle-reclaim.json)에 예약·allocator·프로세스 메모리를 구분해 저장했다. 취소 후 KV가 비어 있고 가중치는 유지되며, 재실행 결과가 같은 것도 실제 모델로 검사했다.

추가 변경의 최종 검증: Core 단위 검사 143개(수동 실행용 7개 제외), 도구·터미널·자원 예약 통합 검사 42개, 실제 모델의 유휴 회수·취소 정리·기존 대기/재개 검사 각각 1개, Loom 자체 검사 792개 통과. release 실행 파일 빌드도 성공했다. 초회 전체 검사에서 기존 HTTP catalog fixture가 읽기 오류로 1회 실패했고, 해당 검사 단독 재실행과 전체 재실행은 모두 통과했다. 관련 HTTP 코드는 변경하지 않았으며 이 변경과의 연관성은 확인되지 않았다.

## 검증

새 `resources_gate`는 실제 shell·PTY로 다음을 검사한다.

1. 예약 반환과 영구적으로 들어갈 수 없는 요청 거부.
2. 대기 도구 우선 처리, 반환 통지 후 추론 재개.
3. 예약을 반환하는 다른 작업 없이도 취소·deadline으로 대기 종료.
4. 실제 Runner가 실행 중 예약을 유지하고 종료 뒤 추론을 깨움.
5. 자원 대기 시간이 도구 timeout에 포함되고 취소된 명령이 시작되지 않음.
6. PTY 응답을 반환한 뒤에도 예약 유지, supervisor 취소·timeout 뒤 반환.

내부 검사는 scratch→resident 원자적 전환과 예상치 초과 기록을 검사한다. Native MoE 검사는 F16/Q4 KV에서 분할 prefill+다음 토큰과 전체 재계산 결과가 같은지 확인한다.

실제 Qwen3-4B-4bit 검사에서는 약 3.207GB의 예약 예산, 512MiB 도구 예약으로 여유를 작게 설정했다. 도구는 Rust 테스트를 컴파일·실행한 뒤 검증용 완료 신호를 기다렸다. 추론은 자원 queue에서 대기했고 도구 종료 뒤 재개했다. 재개 후 greedy 출력과 스트리밍 결과는 기준 결과와 같았다. 이때 별도 모델 요청은 mutex를 기다리는 동안 50ms deadline으로 종료했다.

결과는 [resource-admission.json](../bench/mlx-memory-moe/resource-admission.json)에 저장했다. **이 검사는 대기·재개 회귀 검사다. 대기 신호를 인위적으로 유지하므로 속도 향상이나 실제 동시 작업의 메모리 절감량을 측정한 benchmark로 사용하지 않는다.** 관측된 예약 초과는 0회였다.

별도 [기본 설정 실행 기록](../bench/mlx-memory-moe/resource-bench.json)의 greedy 128토큰은 기존 activation 비교 6회와 모두 일치했다. 동시에 회귀 검사와 빌드가 진행된 실행이므로 속도 차이를 평가하는 자료로 사용하지 않는다. 일반 도구·터미널·worker·agent·신규 예약 통합 검사 118개와 Loom 자체 화면 검사 792개도 통과했다.

재현:

```sh
cargo test --release --manifest-path loom-core/Cargo.toml --test resources_gate -- --test-threads=1

LOOM_MLX_SMOKE=mlx-community/Qwen3-4B-4bit \
LOOM_RESOURCE_SMOKE_OUTPUT=/tmp/loom-resource-admission.json \
cargo test --release --manifest-path loom-core/Cargo.toml --lib \
  mlx_tool_reservation_pauses_and_resumes_inference -- --ignored --test-threads=1 --nocapture

c++ -std=c++20 -O2 -I loom-core/vendor/mlx/include \
  loom-core/native/mlx/tests.cpp -L loom-core/vendor/mlx/lib -lmlx \
  -Wl,-rpath,"$PWD/loom-core/vendor/mlx/lib" -o /tmp/loom-resource-native-tests
/tmp/loom-resource-native-tests

LOOM_MLX_SMOKE=mlx-community/Qwen3-4B-4bit LOOM_MLX_CACHE_MB=64 \
LOOM_RECLAIM_SMOKE_OUTPUT=/tmp/loom-idle-reclaim.json \
cargo test --release --manifest-path loom-core/Cargo.toml --lib \
  mlx_idle_reclaim -- --ignored --test-threads=1 --nocapture

LOOM_MLX_SMOKE=mlx-community/Qwen3-4B-4bit \
cargo test --release --manifest-path loom-core/Cargo.toml --lib \
  mlx_cancel_releases_session_state -- --ignored --test-threads=1
```

Windows/Linux 자원 API와 경로는 조건부 코드에 포함했지만 이번 실제 MLX 검증 장치는 Apple M4 Pro다. 빌드된 Loom을 사용하려면 기존 창을 종료하고 다시 실행한다.
