# Loom Core

UI에 의존하지 않는 Native Core. [설계안](../docs/loom-core-design.md)의 L0–L7을
단일 Worker 경로로 구현했다.

## 실행

저장소 루트에서 실행하며, `--cwd`에는 작업할 Git 저장소의 루트를 지정한다.

```sh
# Native TUI: 검증 후 화면에서 승인해야 원본에 적용
scripts/loom --backend native --cwd /path/to/repo

# Native CLI: 검증을 통과한 제안은 자동 적용
scripts/loom run "수정 요청" --cwd /path/to/repo --json

# 적용 전 검토와 이후 승인
scripts/loom run "수정 요청" --cwd /path/to/repo --no-approve --json
scripts/loom approve TASK_ID --cwd /path/to/repo --json
scripts/loom resume TASK_ID --cwd /path/to/repo --no-approve --json
scripts/loom status --cwd /path/to/repo

# 파일을 바꾸지 않는 분석 작업
scripts/loom run "구조를 분석해줘" --cwd /path/to/repo --intent inspect --json

# 변경이 필수인 작업: 텍스트 답변만으로 완료 판정하지 않음
scripts/loom run "버그를 수정해" --cwd /path/to/repo --intent edit --json

# 검증 명령을 명시해야 하는 저장소
scripts/loom run "수정 요청" --cwd /path/to/repo --check "make test"
```

CLI 종료 코드는 Accepted일 때 0, 미해결·승인 대기·검증 실패일 때 2,
실행 오류일 때 1이다. Rust는 build와 test, Node는 npm test, Python은 pytest를
기본 검증으로 선택한다. 다른 소스 저장소는 `--check`를 지정해야 한다.
`--max-steps`는 Worker 시도별 단계 제한이고, `--max-seconds`와
`--max-tokens`는 한 번의 `solve` 호출 전체(Worker 재시도와 검증 포함)에
적용한다. `--max-attempts`는 Core의 시도 횟수 제한이다. 각 검증 명령은
300초와 전체 잔여시간 중 짧은 시간까지만 실행하며, 중단 시 자식
프로세스 그룹까지 종료한다. 명시적 `resume`은 새로운 실행 예산을 받는다.
`--intent auto|answer|inspect|edit`는 작업의 완료 계약을 지정한다.
`edit`는 실제 수정이 없으면 성공으로 기록하지 않는다. `inspect`와 `answer`는
읽기 전용이며, 생략한 `auto`는 기존 대화 동작을 유지한다.
`--read-only`는 읽기·분석 결과만 허용한다. 기존 편집 작업의 resume 또는 approve도
차단한다. Native backend에서도 같은 제한을 적용한다.

모델 비용 상한은 사용자 공급자 정책을 따른다. 등록 전 기본은 로컬·무료·구독이다.
유료 연결을 등록하면 해당 비용 유형까지 허용하며 `--max-cost`로 더 낮출 수 있다.
[공급자 연결 안내](../docs/loom-provider-connections.md)에서 등록·정책·OAuth·설치를 설명한다.
API key만으로 무료 여부를 확인할 수 없는 Gemini/Groq는 unknown으로 취급한다.
`--tier local|small|medium|flagship`은 필요한 최소 tier를 지정한다.

원격 모델 호출의 기본 상한은 첫 호출 90초, 후속 호출 45초다.
`LOOM_MODEL_CALL_TIMEOUT_SECONDS`로 호출당 제한을 명시할 수 있으며 전체
실행시간 상한은 그대로 적용된다. OmniRoute가 성공한 연결 ID를 응답 헤더로
제공하면 같은 Worker 시도의 후속 호출에 전달한다. 오류가 발생하면 해당
고정을 해제한다. 응답 헤더가 제공한 upstream 시간과 Core가 측정한 실제
소요시간의 차이를 별도 원장에 기록한다. 취소는 모델 응답을 기다리는 Worker에
즉시 전달되지만, 이미 진행 중인 동기 HTTP 소켓은 자체 제한시간까지
살아 있을 수 있다. 실제 upstream 작업의 즉시 취소는 아직 보장하지 않는다.

## 구현 상태

| 계층 | 구현 | 검증 |
| --- | --- | --- |
| L0 base | raw working revision, artifact, EventLog, phase 비용 | 필터·CRLF·binary·mode·symlink 보존, index/HEAD 보존, 이벤트 중단·동시 쓰기 |
| L1 world | 불변 snapshot, 증분 graph, 공유 질의 | 변경 파일만 재파싱, 4827 ID / 5028 span parity |
| L2 effects | Overlay, raw Patch, Runner | 원본 격리, 충돌 보존, 프로세스 그룹 취소, warm build gate 통과 |
| L3 models | 기존 adapters/router 이관, ScriptedModel, 비용 상한 | tool-call 왕복, paid 차단, 금지 경로 거부·cooldown |
| L4 worker | 격리된 단일 Worker, 제안·미해결·실패 결과 | read→edit→finish, budget, 취소; 실모델 baseline 품질 gate 미통과 |
| L5 context | 결정적 계약, bounded slice, scope 보고 | 결정성·상한·범위 확장; token 효율 gate 미검증, 기본 비활성 |
| L6 control | 검증, 승인, raw 3-way 적용, 재시도 인계, 단조 승격, 이벤트 복구 | false finish 거부, 다른 창 승인 경합, 강제 종료 후 재개 |
| L7 surfaces | NativeBackend, loom CLI, bench 연결 | Native 승인·적용 PTY, App 폭별 표시, CLI 프로세스 간 재개 |

Context Compiler는 `--with-context`로만 켠다. 원격 input token −50%와
통과 하락 ≤2 gate가 확인되기 전에는 기본 채택하지 않는다. 2026-10-04 사용자의
그래프 기반 자동 배분 요구에 따라 기존 anchor에서 독립된 2–4개 쓰기 범위를
Core가 직접 병렬 실행하도록 연결했다. 연결된 범위·불명확한 대상·호출 몫 부족은
Single로 남는다. 자식의 비용을 합산하며 병합 뒤 기존 독립 검증과 승인을 지킨다.
측정된 일반 성능 이득을 주장하지 않는다. [실행 감사와 한계](../docs/loom-agent-session-audit-2026-10-04.md)를 참고한다.

Worker 대화와 별도로 Core가 관측한 작업 근거를 WorkMemory에 저장한다.
압축·Worker 교체 뒤에도 제한된 projection과 원문 참조를 전달하며, 오래된 코드
근거는 possibly stale로 표시한다. Worker의 finish는 제안 종료다. 실제 patch가
Core 검증에서 실패하면 남은 예산으로 복원해 수리하고, 동일 실패 patch는 다시
검증하지 않는다. 명시적 unresolved와 max_attempts=1은 이 동작을 제한한다.
현재 한계와 품질 판정 기준은 [실행 계약](../docs/loom-core-execution-contract.md)에 있다.

Native TUI는 텍스트 작업, 변경 승인·거절·중단, 모델 tier, 작업 이력과 비용을
지원한다. 이미지·skills·Codex 원격 session 관리 등은 명시적으로 지원하지 않는다.
기존 Codex backend와 demo의 UI 기능은 유지한다. `loom-tui --run`은 Native CLI로
위임하고, `scripts/loom-enginebench --legacy --engine PATH`로 구 엔진을 비교할 수 있다.

## 저장과 적용

- `.loom/artifacts/<sha256>`: 원문과 WorkerResult, Patch의 내용 기반 저장.
- `.loom/events.jsonl`: 순번을 붙인 append-only JSONL. 기록 후 fsync하며 마지막
  불완전 기록만 복구한다. 완성된 손상 기록은 오류로 반환한다.
- revision: 임시 index와 raw blob으로 작업 폴더를 기록한다. 사용자 index/HEAD는
  바꾸지 않고 `.loom/`, `.custom-tui/`, ignored 파일을 제외한다. tracked 파일은
  ignore 설정에도 포함한다. `refs/loom/revisions/<tree>`로 Git GC에서 보존한다.
- Overlay: captured tree의 detached worktree를 checkout 없이 생성해 raw blob을
  복원한다. clean/smudge 필터를 실행하지 않는다. lock으로 동시 slot을 분리하고
  순차 실행에서는 경로를 재사용한다. Cargo target은 `.loom/target/`에 공유한다.
- Patch: base를 재확인하고, Git 저장소 밖의 임시 사본에서 raw diff를 검증한다.
  base가 이동하면 파일별 base/현재/제안을 raw bytes로 3-way 병합한다. 충돌은
  격리된 사본에만 남는다. 병합 결과도 다시 검증한 뒤 Core가 원본에 적용한다.
- 재시도는 이전 제안을 새 Overlay에 복원하고 Worker·Core 검증의 원문 artifact를
  이어받는다. Context Compiler가 켜져 있으면 수정된 사본에서 slice를 다시 만든다.
  충돌 후 명시적으로 resume하면 최신 사용자 revision을 새 base로 삼는다.
- 실행·재개는 작업별 lease를 사용해 동일 작업의 중복 실행을 거부한다.
  프로세스 종료 시 OS가 lease를 해제한다. 일반 파일의 기존 읽기·쓰기 권한은
  patch 적용 후에도 보존하고 Git이 기록하는 실행 비트 변경만 반영한다.
- 적용 직전 expected revision을 기록한다. 적용 직후 프로세스가 끊기면 재시작 시
  실제 revision과 대조해 완료 상태를 복구한다. 승인·거절은 동일한 apply lock과
  최신 EventLog를 사용해 다른 창의 오래된 상태가 완료를 덮어쓰지 못한다.
- Overlay는 파일 작업 사본 격리다. Runner는 저장소 build/test 명령을 실행하며
  별도의 OS sandbox는 구현하지 않는다. Unix 취소를 검증했다. submodule은 명시적
  오류로 거부하며 Windows symlink와 프로세스 취소의 동등성은 아직 검증하지 않았다.

## 검증

```sh
cargo test --manifest-path loom-core/Cargo.toml -- --test-threads=1
cargo clippy --manifest-path loom-core/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path loom-core/Cargo.toml --test ripgrep_parity -- --ignored
cargo run --manifest-path loom-core/Cargo.toml --example overlay_gate -- /path/to/git-rust-repo
cargo build --manifest-path loom-tui/Cargo.toml
cargo test --manifest-path loom-tui/Cargo.toml -- --test-threads=1
cargo run --manifest-path loom-tui/Cargo.toml -- --self-check --screens
python3 loom-tui/tests/terminal_smoke.py
```

parity corpus가 다른 위치에 있으면 `LOOM_PARITY_REPO`를 지정한다.
Overlay 측정은 `.loom/target/`이 없는 별도 사본에서 시작해야 한다.
실행 중인 TUI는 재빌드만으로 바뀌지 않는다. Ctrl+Q로 종료하고 다시 실행한다.

최종 검증: Core 79개, TUI 189개 테스트와 self-check 362개가 통과했다.
Clippy는 모든 target에서 경고 없이 통과했고, PTY 기본 suite 29개 시나리오가
통과했다. 외부 corpus parity는 별도로 확인했으며, 모델을 호출하는 live 테스트와
desktop 창 테스트는 자동 suite에서 제외했다. Native 승인·적용, 검증 중 종료와
코드 되돌리기를 실제 PTY에서 확인했다.

## 측정 기록 (2026-10-02, macOS ARM64, debug)

[구조화 기록](measurements/2026-10-02.json)에 조건과 결과를 보존했다.

- ripgrep `3fce3b5bb0236da2df6d99672afb8a719642eca7`: Rust 파일 110개,
  1,902,104 bytes, graph 생성 1,384ms. 4827 ID / 5028 span oracle 일치.
  필드·variant 1191개는 legacy 비교에서 제외한다. parity harness 최대 RSS는
  약 54 MiB이며 oracle 사본까지 포함한다.
- 초기 Overlay는 경로가 매번 달라 build 9,973/2,224ms, 비율 22.3%로 실패했다.
  안정된 slot 경로로 바꾼 후 10,052/98ms로 통과했고, raw 복원 경로를 포함한
  재측정은 생성 84/78ms, build 12,908/116ms, warm 비율 **0.90%**였다.
- 실모델 pilot은 각 12개, jobs=3, task timeout=180초, Worker budget=150초,
  max-steps=30으로 실행했다. Ollama Gemma E4B Q4 모델의 요청이 시간 초과됐다.
  구 엔진 숨은 테스트는 2/12지만 완료 0건, Native는 0/12·완료 0건이었다.
  Native는 미수락 patch를 원본에 남기지 않으므로 부분 편집 통과와 같은 의미가
  아니다. Native false_finish=0도 수락 0건인 이 조건에서 품질 증명이 아니다.
  Native 실행 초반에 중복 bench가 잠시 겹쳐 종료했다. 이 pilot은 통제된 성능
  비교로 사용하지 않으며 L4 parity, L5 token, L6 실제 승격 효율 gate는 미확인이다.
