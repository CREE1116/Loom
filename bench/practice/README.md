# Loom 실전 연습 문제

설명뿐 아니라 실제 수정·검증을 끝내는지 확인하는 작은 문제 세 개다.
Rust 표준 라이브러리만 사용하며 모든 테스트를 offline으로 실행할 수 있다.

| 문제 | 요청 | 확인할 능력 |
|---|---|---|
| [ttl-boundary](tasks/ttl-boundary/task.md) | 만료 시각에 값이 한 번 더 반환되는 캐시 버그 수정 | 원인 탐색, 시간 경계, 기존 동작 보존 |
| [job-report](tasks/job-report/task.md) | 여러 작업 기록의 집계와 한국어 요약 구현 | 여러 파일 작업, 분해 판단, 미보고 토큰 보존 |
| [retry-context](tasks/retry-context/task.md) | 일시적 모델 오류·편집 오류에서 작업을 이어가는 실행 루프 수정 | 오류 피드백, 문맥·편집 보존, 유한한 복구 |

각 문제는 `task.md`(Loom에 보낼 요청), `repo/`(시작 코드와 공개 테스트),
`hidden/`(추가 채점 테스트), `reference/`(검증용 정답 구현)로 구성된다.
**Loom의 작업 폴더에는 `repo/`만 복사한다.** 숨은 테스트와 정답을 같이 넣으면
독립적인 채점이 되지 않는다. 시작 코드는 컴파일되지만 일부 테스트가 의도적으로 실패한다.

## 직접 TUI로 풀기

실행 스크립트가 시작 코드만 독립 폴더에 복사하고, 문제 설명을 `TASK.md`로 넣고,
Git을 준비한 뒤 Loom V3를 연다. 숨은 테스트와 정답은 복사하지 않는다.

어느 디렉토리에서든 다음 명령으로 실행한다. 번호는 1 캐시 버그, 2 작업 요약,
3 실패 복구이며 생략하면 첫 문제다.

```sh
scripts/loom-practice 1
```

Loom이 열리면 다음 한 줄을 채팅에 입력한다.

```text
TASK.md의 요구사항대로 수정하고 테스트로 검증해줘.
```

준비한 폴더 경로는 실행 전에 출력한다. 창을 닫아도 폴더를 유지하며,
그 안의 `./run-loom`으로 같은 작업 폴더에서 다시 실행할 수 있다.
`run-loom`은 호출한 터미널의 위치와 관계없이 자신이 있는 예제 폴더를 사용한다.

```sh
# 저장소 루트에서 실행하는 경우
scripts/loom-practice 2

# 원하는 새 경로에 준비. 이미 있는 폴더에는 덮어쓰지 않는다.
scripts/loom-practice 3 --directory /tmp/loom-recovery-example

# 준비만 하고 Loom은 나중에 열기
scripts/loom-practice 1 --prepare-only
```

수동으로 준비하려면 저장소 루트에서 다음 명령을 사용한다.

```sh
practice_dir=$(mktemp -d /tmp/loom-practice.XXXXXX)
cp -R bench/practice/tasks/ttl-boundary/repo "$practice_dir/repo"
git -C "$practice_dir/repo" init -q
scripts/loom-v3 --cwd "$practice_dir/repo"
```

[첫 번째 문제 설명](tasks/ttl-boundary/task.md)의 내용을 채팅에 붙여 넣는다.
나머지 문제는 복사 경로의 `ttl-boundary`만 해당 문제 이름으로 바꿔 새 폴더에서 시작한다.
완료되면 수정 제안·검증 결과를 확인한 뒤 승인한다. 원본에 적용된 작업 폴더에서
`cargo test --offline`로 공개 테스트를 실행할 수 있다.

### 이미 예제 작업 폴더에 들어온 경우

예제는 Loom으로 수정할 대상 코드다. 예제 안의 `cargo run`은 Loom을 실행하지 않는다.
`scripts/loom-v3`도 Loom 저장소 루트를 기준으로 한 상대 경로이며 전역 명령으로
등록돼 있지 않다면 `loom-v3`만 입력해도 실행되지 않는다.

예제 작업 폴더에서 **Loom 저장소의 스크립트를 절대 경로로 호출**하고 현재
폴더를 `--cwd`로 지정한다. 이 컴퓨터의 저장소 위치에서는 다음 명령이다.

```sh
scripts/loom-v3 --cwd "$PWD" --new
```

다른 위치에 Loom을 받았다면 앞부분을 해당 저장소의 절대 경로로 바꾼다.
복사한 독립 작업 폴더에서 실행해야 시작 코드와 채점용 원본을 보존할 수 있다.

## V1/V2/V3 또는 모델별로 비교하기

기존 `scripts/loom-corebench`가 같은 초기 코드를 버전별 독립 폴더로 복사하고,
원본 보존 → 제안 검증 → 승인 → 숨은 테스트 순서로 채점한다.

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
scripts/loom-corebench \
  --tasks bench/practice/tasks \
  --versions v3 --repeat 3 --jobs 1 \
  --model omniroute/auto \
  --binary custom-tui/target/release/custom-tui \
  --max-calls 20 --timeout 240 \
  --json bench/practice/results/v3-auto.json
```

버전을 비교할 때는 `--versions v1 v2 v3`로 바꾼다. 모델별 비교는 `--model`과
결과 파일을 바꾸되 나머지 조건을 맞춘다. 기본 실행기를 그대로 사용하므로 호출,
토큰 미보고 여부, 실제 사용 모델, 시간, 승인 전 원본 보존, 숨은 테스트 결과를 남긴다.
결과와 실행 폴더의 위치는 실행기가 출력한다. 반복 실험의 임시 실행 폴더는 결과
보관 여부를 결정한 다음 해당 폴더만 정리한다.

`job-report`는 독립된 두 구현 파일을 갖지만 병렬 워커를 강제하지 않는다.
단일 실행과 분해 실행의 관측 결과로 판단해야 한다. `retry-context`는 **오류 복구
코드를 작성하는 능력**을 채점하며, Loom 자체의 HTTP 장애 복구를 주입하는 실험은
아니다. 실제 Core의 HTTP 복구는 `custom-tui/tests/model_recovery_terminal_smoke.py`로
별도로 재현한다.

## 문제 자체의 검증

```sh
python3 bench/practice/verify-fixtures.py \
  --json bench/practice/results/fixture-validation.json
```

임시 폴더에서 초기 코드·채점 테스트가 컴파일되는지, 초기 코드가 숨은 테스트에
실패하는지, 분리된 정답이 공개·숨은 테스트를 모두 통과하는지 확인한다.
외부 모델은 호출하지 않는다. 문제 파일 자체에 빌드 결과나 모델 캐시를 저장하지 않는다.

이 세 문제의 통과만으로 일반적인 코딩 능력이나 Codex와의 동등성을 증명할 수는 없다.
먼저 작고 재현 가능한 기준으로 쓰고, 실제 사용 중 실패한 요청을 이후에 추가한다.
