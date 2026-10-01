# Loom 구현 기획서

상태: 구현 설계 기준 · 2026-10-01. 사용자 기획 및 후속 확인을 반영한다. 현재 구현 현황과 목표 아키텍처는 구분한다. 이 문서와 기존 TUI 초안이 충돌하면 이 문서를 우선한다.

> Core는 세계를 기억하고, Worker는 문제를 푼다.

## 1. 프로젝트 정의와 최종 목표

Codex CLI를 출발점으로, 비용과 능력이 다른 여러 지속 세션을 하나의 Orchestrator Core가 병렬 운용하여 API 비용과 작업 시간을 동시에 줄이는 코딩 에이전트 CLI를 만든다.

**최종 목표는 Codex 코어를 단계적으로 자체 Core와 자체 Runtime으로 완전히 교체하는 것이다.** 기존 Codex runtime은 초기 구현과 호환성 검증을 위한 과도기 기반이다. 영구적인 필수 의존성으로 두지 않는다. 처음부터 sandbox, approval, exec, tool plumbing을 모두 다시 만들지는 않는다.

에이전트는 Core 하나다. LOCAL / SMALL / MEDIUM / FLAGSHIP은 Core가 사용하는 계산 자원(worker)이며, 자율적인 에이전트 계층이 아니다.

최적화 목표:

```text
Task Quality ≈ Vanilla Codex Baseline
API Cost ↓
Wall-clock Latency ↓
Uncached Remote Tokens ↓
Redundant Work ↓
```

품질 저하를 비용 절감으로 포장하지 않는다. cached input까지 포함한 총 토큰 감소만으로 성공을 판단하지 않는다.

## 2. 반드시 유지할 원칙

1. Source of Truth는 Core다. TaskGraph, repository revision, accepted patches, findings, decisions, tests, dependencies, worker 상태, memory, cost를 소유한다.
2. Conversation, Canonical State, Shared Memory, Archive를 분리한다. worker 대화는 계산 과정이며, 다른 worker에게 전체 history를 복사하지 않는다.
3. 한 번 얻은 정보는 Finding / TestResult / Decision / Failure / Patch로 보관한다. 유효한 증거를 재사용하고, 코드 변경으로 무효화된 정보는 다시 검증한다.
4. 모델 능력과 권한은 별개다. FLAGSHIP도 최종 적용·병합·취소·재시도 권한을 갖지 않는다.
5. worker 사이의 직접 통신을 금지한다. 모든 결과와 후속 요청은 Core를 통한다.
6. 독립 Task만 병렬 실행한다. 같은 사용자 요청을 네 worker에 broadcast하지 않는다.
7. 최종 patch 적용, 충돌 해결, routing, escalation은 Core의 책임이다. 모델의 계획·판단은 검증할 proposal 또는 recommendation이다.
8. Activity Panel 없이도 입력, 실행, 승인, 중단, 결과 확인을 끝까지 수행할 수 있어야 한다.

## 3. 현재 구현과 목표 구조

현재 저장소에는 독립 `custom-tui` crate가 있고, `AgentEvent` / `AgentCommand` 경계로 UI와 backend를 분리했다. Codex JSON-RPC와 item 해석은 `backend/codex.rs`와 `backend/codec.rs`가 맡고, UI는 `app/events.rs`의 typed reducer로 상태를 갱신한다. `transport.rs`의 `Event::Message(Value)`는 adapter 내부 전송 이벤트다. `MockCore`도 같은 UI를 구동한다. 초기 자체 Core는 비동기 LOCAL 공유 코드 탐색을 처리하며, 전체 TaskGraph와 원격 실행 제어는 아직 구현 중이다.

현재 모듈:

| 파일 | 현재 책임 | 다음 경계 |
| --- | --- | --- |
| `custom-tui/src/engine.rs` | 문자 셀·입출력·클릭 영역 | UI 엔진 유지 |
| `custom-tui/src/app.rs` · `app/events.rs` | 대화·도구·승인·활동 projection | 도메인 이벤트로 UI 상태 갱신 |
| `custom-tui/src/main.rs` | 입력·도메인 이벤트 루프 | 사용자 command 전달 및 이벤트 수신 |
| `custom-tui/src/backend/*` | Codex adapter와 독립 Mock Core | protocol·request correlation·원격 실행 호환 |
| `custom-tui/src/core/*` | 초기 LOCAL 실행과 공유 코드 탐색 | CanonicalState·TaskGraph·worker 제어로 확장 |
| `custom-tui/src/transport.rs` | WebSocket JSON-RPC | Codex adapter 내부 전송 계층 |
| `custom-tui/src/runtime.rs` | Codex 서버 시작·접속 | 초기 Codex backend 연결 |
| `custom-tui/src/window.rs` | 창 실행·클립보드 | UI 주변 기능 유지 |

교체 경로:

```text
현재: UI → Codex app-server / Codex Core
1단계: UI → AgentEvent / AgentCommand → Codex Adapter
2단계: UI → 자체 Orchestrator Core → Runtime Adapter → Codex Runtime
3단계: UI → 자체 Orchestrator Core → Runtime Adapter → 단계별 자체 Runtime
최종: UI → 자체 Orchestrator Core → 자체 Runtime → OS / Model Providers
```

`AgentEvent`와 `AgentCommand`는 제품 경계다. Codex RPC method, thread ID, item type이 UI와 Core의 도메인 계약으로 새어 나오지 않게 한다. legacy ID가 필요하면 adapter가 도메인 ID와 매핑한다.

Codex adapter에 계획·memory·routing 정책을 넣지 않는다. Codex backend가 수행하는 내부 자율 동작을 자체 Core의 통제로 착각하지 않는다. 과도기에 남은 통제 범위는 capability와 구현 현황으로 명시한다.

## 4. Core와 worker의 책임

Core는 사용자 요청을 받고 TaskGraph를 만든다. 계획 검증, dependency, scheduling, 취소, retry, escalation, context compilation, cache planning, state/memory 저장, patch 검증·병합, revision 관리, 비용·시간 측정, 이벤트, 최종 응답을 담당한다.

| Worker | 역할 | 대표 작업 |
| --- | --- | --- |
| LOCAL | API 비용 없이 실행 가능한 자원 | filesystem, search/index, git, shell, parser, test runner, output processor, 선택적 tiny model |
| SMALL | 저렴하고 명확한 계산 | 기계적 수정, 단순 refactor, 테스트·문서 작성, 구조화·요약 |
| MEDIUM | 일반 코딩의 중심 | 기능 구현, 디버깅, 여러 파일 수정, 보통 설계 판단, SMALL 실패 해결 |
| FLAGSHIP | 어려운 판단 자원 | 복잡한 원인 분석, cross-module reasoning, 반복 실패, 의미적 충돌, 모호한 요구, 고난도 review |

LOCAL은 모델 하나의 이름이 아니다. 예를 들어 ‘테스트 다시 돌려’, ‘Foo 정의 찾아’, ‘git status 봐줘’는 deterministic LOCAL 작업으로 처리한다. 요청의 의도 자체가 모호하면 별도 분석 Task가 필요하다.

초기 routing은 규칙으로 시작한다. 작은 decision model은 trace가 쌓인 뒤 도입한다. worker tier는 공급자·모델 ID와 분리하고, SMALL과 MEDIUM이 같은 모델을 사용해도 구조가 유지되어야 한다.

## 5. 주요 데이터 계약

다음 필드는 의미 계약이다. Rust의 구체 ID 타입, 저장 형식, async trait 방식은 구현 단계에서 정한다. 예시는 컴파일 가능한 완성 API가 아니다.

```text
Task {
  id, parent, goal, task_type, status,
  dependencies, dependents, assigned_worker,
  inputs, outputs, base_revision, priority, cost_budget, created_at
}

WorkerTask {
  task_id, goal, task_type, context, constraints, evidence_refs,
  base_revision, allowed_capabilities, output_contract
}

WorkerResult {
  task_id, worker, base_revision, status, summary,
  findings[], decisions: DecisionProposal[], patches[], tests[],
  touched_files[], artifacts[], unresolved[], confidence,
  escalation_request
}

CanonicalState {
  goal, repo_revision, tasks,
  accepted_findings, accepted_decisions, patches, tests, failures,
  constraints, artifact_index, worker_states, cost_state, event_log
}

Finding {
  id, content, base_revision, related_files[], related_symbols[],
  evidence_refs[], confidence, validity
}

TestResult {
  command, exit_code, passed, failed, primary_errors[],
  relevant_lines[], raw_output_ref
}

Recommendation { preferred_proposal, rationale, risks[], confidence }
RoutingDecision { worker, parallelizable, confidence }
```

Task type: `Explore / Implement / Debug / Review / Plan / Arbitrate / Test / Summarize`.

Task status: `Pending / Ready / Running / Blocked / Completed / Failed / Cancelled / NeedsReview`.

Finding validity: `Valid / PossiblyStale / Invalid`.

DecisionProposal이 accepted Decision이 되는 시점과 Patch가 accepted Patch가 되는 시점은 Core가 결정한다. worker의 `Completed` 응답만으로 repository 반영이나 전체 사용자 작업 완료를 선언하지 않는다.

주요 인터페이스의 책임:

```rust
trait Worker {
    async fn execute(&mut self, task: WorkerTask) -> WorkerResult;
}
trait ContextCompiler {
    fn compile(&self, task: &Task, worker: WorkerKind,
               state: &CanonicalState) -> WorkerContext;
}
trait Scheduler {
    fn schedule(&self, state: &CanonicalState) -> Vec<ScheduledTask>;
}
trait ConflictResolver {
    async fn resolve(&self, conflict: Conflict,
                     state: &CanonicalState) -> ConflictResolution;
}
trait AgentEventSink {
    fn emit(&self, event: AgentEvent);
}
```

UI의 결과 수신 경계는 `AgentEvent`다. 사용자 입력은 별도 `AgentCommand`로 제출·중단·승인·세션 조작 등을 전달한다. UI에서 직접 worker를 호출하거나 TaskGraph를 수정하지 않는다.

## 6. TaskGraph와 실행 수명

Core가 사용자 요청마다 ROOT와 DAG를 소유한다. 모델이 제안한 plan은 Core가 cycle, dependency, capability, budget을 검증한 뒤 materialize한다.

예: 로그인 오류 수정은 LOCAL 탐색·재현 → MEDIUM 원인 분석 → 수정 → LOCAL 회귀 검증으로 이어진다. 문서 조사처럼 독립인 작업만 동시에 실행한다. 원인 분석 결과가 수정 제약을 결정한다면 수정 Task는 해당 결과를 기다린다.

구현 보완 규칙:

- `parent`는 표시·그룹 구조이며 실행 dependency를 대신하지 않는다.
- 준비 여부는 의존 결과의 acceptance와 capability·자원·budget으로 계산한다. Blocked에는 dependency ID와 이유를 남긴다.
- 재시도와 escalation의 attempt를 구분한다. Task ID 외에 attempt ID를 기록하여 오래된 결과를 현재 실행 결과로 적용하지 않는다.
- dependency 실패·취소 시 하위 작업을 자동 성공시키지 않는다. 재계획·차단·취소 중 Core 정책을 적용한다.
- 실행 중단 요청과 중단 확인을 구분한다. 뒤늦게 도착한 결과도 보관할 수 있지만 취소된 patch를 자동 적용하지 않는다.
- ROOT 완료는 필요한 결과 acceptance와 validation 완료를 의미한다. 일부 Task 성공만으로 완료하지 않는다.

## 7. 지속 세션과 cache

SMALL / MEDIUM / FLAGSHIP은 독립 persistent session을 갖는다. 각 session의 history는 작업 연속성과 context cache 재사용을 위한 계산 기록이다. 다른 session history는 자동 공유하지 않는다.

```text
Stable Prefix
  System / Worker policy / Tool protocol
  Project instructions / Stable task contract
Append-only Area
  Task / Result / Finding / Repo delta / ...
```

tool 순서나 system prompt를 매번 바꾸지 않고, 매 턴 요약을 prefix에 삽입하지 않는다. context threshold에 도달할 때 CacheEpoch을 바꾸고 한 번 compaction한다. policy·tool contract·project instruction 변경도 새 epoch의 이유로 기록한다.

구현 보완 규칙: 하나의 mutable session에는 한 번에 하나의 turn만 실행한다. 같은 tier에서 동시 원격 실행이 필요하면 별도 session을 명시적으로 만들고 비용과 cache 분산을 기록한다. LOCAL은 독립 job을 여러 개 실행할 수 있다. persistent session 유지가 공급자의 실제 cache hit를 보장하지 않으므로 사용량으로 검증한다.

세션을 잃어도 durable Canonical State와 관련 evidence에서 필요한 context를 재구성할 수 있어야 한다. 전체 과거 대화 replay를 복구의 필수 조건으로 삼지 않는다.

## 8. Memory와 Context Compiler

| 계층 | 내용 | 제공 방식 |
| --- | --- | --- |
| HOT | goal, revision, accepted patch, 제약, 실행 Task, 현재 오류 | Core가 직접 제공 |
| WARM | Finding, Decision, FailurePattern, architecture fact, 해결된 충돌 | hybrid retrieval |
| COLD | 원문 shell output, 옛 conversation·diff·test log·snapshot | artifact ref로 필요 부분 조회 |

retrieval은 symbol match, file overlap, task/test relation, revision validity, dependency relation, semantic similarity를 함께 활용한다. 모든 정보를 vector DB로 보내지 않는다.

Finding은 provenance를 갖는다. 관련 파일이 바뀌면 `PossiblyStale`로 바꾸고, 재검증 실패 시 `Invalid`로 처리한다. 오래된 기억을 확정 사실처럼 전달하지 않는다. 무관한 파일 변경만으로 모든 기억을 폐기하지 않는다.

Context Compiler는 전체 상태에서 Task별 projection을 만든다: goal, constraints, relevant findings/decisions/code/errors, 필요한 parent/dependency results, repo delta. evidence ref를 유지하며, 무엇을 왜 context에 넣었는지 추적한다.

## 9. Tool Output Virtualization

Runtime 결과 원문은 artifact store에 한 번 보관한다. LOCAL parser가 TestResult 등으로 변환한다. 모델에는 실패 수, 주요 오류, 관련 줄, 원문 ref를 제공한다.

```text
cargo test FAILED
2 failures: auth::expired_session, session::refresh
Primary error: expected 302, got 401
Raw: result://test/1832
```

원문 일부 조회는 ref와 범위를 지정한다. 출력 절단 여부와 parser 불확실성을 기록한다. parser가 이해하지 못한 로그를 성공으로 취급하지 않는다. tool output를 UI에서 접는 것만으로 원격 입력 비용이 줄었다고 판단하지 않는다.

## 10. Repository, patch와 충돌

canonical repository는 하나다. Task는 `base_revision`을 받으며 worker는 격리된 worktree 또는 snapshot에서 작업하고 Patch를 반환한다. canonical tree 직접 수정 권한은 없다.

Core가 patch·base·허용 경로·검증 결과를 확인한 뒤 `R42 + P19 → R43`처럼 반영한다. 동일 base에서 출발한 두 번째 patch는 최신 revision에 대한 적용 가능성과 필요한 validation을 다시 확인한다. 사용자 기존 변경도 revision 기준에 포함하고 덮어쓰지 않는다.

| 충돌 단계 | Core의 처리 |
| --- | --- |
| Structural | 위치·구조가 독립이면 자동 병합 후 검증 |
| Textual | 같은 줄 등 충돌에 deterministic / 3-way merge 시도 |
| Behavioral | 격리된 조합에서 tests, lint, typecheck, static validation |
| Semantic | 검증으로 구분되지 않는 설계 의미를 MEDIUM / FLAGSHIP에 판단 요청 |

```text
Core → ArbitrationRequest → MEDIUM / FLAGSHIP
     ← Recommendation
Core → constraints·evidence 검증 → final decision → apply
```

arbitration은 read-only이며 recommendation만 반환한다. Core는 판단 근거·risks·confidence·채택/거절 이유를 저장한다. validation 통과만으로 semantic equivalence를 보장하지 않는다.

구현 보완 규칙: canonical revision 갱신을 직렬화하고, 검증한 base와 적용 직전 상태가 일치하는지 확인한다. accepted patch, revision, memory invalidation, 이벤트 기록은 일관되게 저장한다. 실패 시 복구 가능한 상태를 남긴다.

## 11. Routing, escalation과 speculative work

초기: 명확한 local action → LOCAL, 기계적 수정 → SMALL, 일반 코딩 → MEDIUM, 고난도·반복 실패 → FLAGSHIP.

escalation trigger는 낮은 confidence, 동일 실패 반복, 범위 확장, 모르는 dependency, 반복 test 실패, semantic ambiguity, explicit worker request다. worker는 상위 worker를 부르지 않고 Core에 요청한다. Core가 필요한 evidence와 실패 기록을 다음 Task에 전달한다. retry/escalation 횟수와 budget을 제한하고 반복 실패를 기록한다.

장기 routing은 difficulty, API price, worker load, cache affinity, 이미 아는 files, expected latency, failure probability를 함께 고려한다. SMALL cold start보다 MEDIUM cached context가 저렴하면 MEDIUM을 선택할 수 있다.

tiny decision model의 범위는 worker 선택, task category, parallelizable 여부, escalation 필요 여부다. 전체 코드를 읽는 설계자 역할을 맡기지 않는다. Rules와 약 0.8B decision model을 같은 trace·benchmark에서 A/B 비교한다.

speculative policy: LOCAL 적극 허용, SMALL 제한 허용, MEDIUM 명시 Task 중심, FLAGSHIP 금지. speculative LOCAL 작업도 자원·중복 실행·현재 revision을 관리한다. 무료 API 작업이라고 wall-clock 비용이 없는 것은 아니다.

## 12. UI와 이벤트

메인 화면은 무엇이 이루어지고 있는지, Activity는 어떻게 이루어지는지 보여준다. 기존 하위 에이전트 목록은 과도기 Codex 기능이다. 최종 Activity의 단위는 Core가 소유하는 Task와 worker이며, tier를 자율 에이전트로 표시하지 않는다.

```text
로그인 오류 수정
├ ✓ 코드 탐색       LOCAL
├ ✓ 실패 재현       LOCAL
├ ● 원인 분석       MEDIUM
├ ◌ 패치 생성       SMALL
│  └ 원인 분석 대기
└ ◌ 회귀 테스트     LOCAL
   └ 패치 생성 대기
```

기호: `● running / ✓ completed / ◌ waiting / ! failed / ↗ escalated / × cancelled`. 모델 label은 LOCAL / SMALL / MEDIUM / FLAGSHIP으로 작게 표시한다.

progressive disclosure:

1. 기본: Task, worker, 상태, 대기 이유.
2. 상세: dependencies, inputs, outputs, elapsed, cost.
3. 디버깅: raw tool calls, worker prompt, context sources, usage/cache, WorkerResult, revision.

critical path는 전체 완료를 막는 dependency 경로를 표시한다. 예상 duration으로 계산한 경로는 추정임을 구분한다. worker running/idle과 동시 job 수, task별 호출·usage·비용·시간도 관찰할 수 있게 한다.

AgentEvent에는 다음 도메인 변화가 포함된다:

```text
UserMessage, AssistantMessage
TaskCreated, TaskStarted, TaskBlocked, TaskCompleted, TaskFailed
WorkerAssigned, WorkerEscalated
ToolStarted, ToolCompleted
FindingAdded, DecisionAdded
PatchProposed, PatchAccepted, PatchRejected
ConflictDetected, ConflictResolved
ModelCallStarted, ModelCallCompleted, CostUpdated
```

구현 보완 규칙: 취소·검토 대기·응답 delta·승인 요청/해결·연결 오류·snapshot/replay에 필요한 계약도 포함한다. 이벤트 envelope에 schema version, sequence, goal/task/attempt 식별자를 둔다. state 반영 후 이벤트를 내보내고 재접속 시 snapshot과 후속 event를 일관되게 제공한다. UI는 중복 이벤트로 실행을 반복하지 않는다.

## 13. 비용 측정과 benchmark

모델 호출마다 model, provider, session, epoch, task/attempt, input, cached input, uncached input, output, price, latency, prefix hash를 기록한다. 호출 실패·retry·escalation 비용도 포함한다. remote worker가 하나인 단계부터 측정한다.

구현 보완 규칙: 총 input에 cached input이 포함되는지 adapter에서 정규화하고 중복 과금 계산을 막는다. 가격은 버전·적용 시점을 저장한다. usage 또는 가격이 없으면 unknown으로 표시하고 실제 $0으로 처리하지 않는다. 추정 비용과 확인된 비용을 구분한다.

동일한 작업·초기 repository·성공 기준으로 Vanilla Codex와 비교한다. 모델·설정·cache warm/cold 조건·반복 실행 조건을 기록한다.

측정: success rate, API cost, wall-clock time, remote calls, uncached/cached input, output, cache hit rate, tool calls, escalations, retries, conflicts.

```text
Cost Ratio = Custom API Cost / Vanilla Codex API Cost
Latency Ratio = Custom Completion Time / Vanilla Codex Completion Time
```

품질 허용 하락폭과 비용·속도 목표치는 benchmark 시작 전에 정한다. 현재 확정된 숫자는 없다. Activity의 baseline estimate와 saved 비율은 비교 가능한 근거가 있을 때 표시하고, 실측 baseline과 추정을 구분한다.

## 14. MVP 구현 순서와 완료 기준

| 단계 | 구현 범위 | 완료 증거 |
| --- | --- | --- |
| MVP 1 | UI/Core 경계: AgentEvent·AgentCommand, Codex adapter | Codex 연결 없이 Mock Core로 대화·stream·tool·승인·중단·오류·활동 등 현재 UI 흐름 구동. 실제 backend 흐름도 유지 |
| MVP 2 | Single Worker Core: Task, TaskGraph, CanonicalState, WorkerTask/Result, revision, event bus, cost trace | 단일 worker 작업을 Core가 생성·검증·완료하고 세션 history 없이 상태 복구 |
| MVP 3 | LOCAL: search/read/git/test/basic shell/output parsing | 명확한 local 요청에서 remote call 0, raw output ref와 구조화 결과 재사용 |
| MVP 4 | persistent remote session abstraction, stable prefix, epoch, cache metrics | SMALL/MEDIUM/FLAGSHIP용 독립 세션 계약과 usage 추적. tier별 실제 routing은 MVP 7 |
| MVP 5 | parallel scheduler, dependency/blocking, Activity TaskGraph | 독립 Task 동시 실행, dependency 선행 실행 방지, 실패·취소 전파와 대기 이유 표시 |
| MVP 6 | patch isolation, merge, conflict, revision invalidation | 같은 base의 격리 patch를 Core만 적용. stale base·충돌·사용자 변경 보존 검증 |
| MVP 7 | rule routing, multi-tier workers, retry/escalation | Core가 tier 선택·승격하고 worker 직접 호출·통신 없이 실패 evidence 전달 |
| MVP 8 | Shared Memory: Finding/Decision/Failure/Evidence, hybrid retrieval | 유효 지식 재사용, 관련 revision 변경에 validity 갱신, stale 사실 오용 방지 |
| MVP 9 | trace dataset, tiny decision model | Rules와 decision model의 품질·비용·latency A/B 결과 |
| MVP 10 | advanced optimizer | cache-affinity/cost-aware scheduling, speculative LOCAL, learned tool model, compaction/ranking을 개별 실측으로 채택 |

세부 개발 우선순위는 UI → event boundary → TaskGraph → CanonicalState → Worker interface → LOCAL → persistent sessions → Activity graph → parallel scheduler → revision/patch isolation → conflict resolver → multi-tier routing → shared memory → cost-aware router → decision model이다.

Router부터 만들지 않는다. patch isolation이 준비되기 전 concurrent canonical writer를 허용하지 않는다. MVP 2에서 legacy backend를 사용한다면 파일 변경 통제의 미완성 범위를 기록하고 MVP 6 완료로 간주하지 않는다.

## 15. Codex Core와 Runtime의 완전 교체

MVP는 제품 기능의 진행 순서다. 완전 교체는 아래 의존성 축으로 병행하며, **MVP 10만으로 최종 목표가 완료되지는 않는다.**

| 교체 단계 | 자체 소유로 옮길 책임 | 완료 기준 |
| --- | --- | --- |
| UI 경계 교체 | domain command/event, UI projection | UI가 Codex protocol 없이 작동 |
| Agent Core 교체 | task planning/control, state, memory, context, routing, merge, cost | 작업 수명과 최종 판단을 자체 Core가 소유. Codex 내부 loop가 상위 통제자로 남지 않음 |
| 모델 실행 교체 | provider adapter, request/response stream, session/epoch, tool-call contract, usage | native model backend로 동일 WorkerTask/Result 흐름 구동 |
| Runtime 기능 교체 | exec/process cancellation, filesystem/search, tool dispatch/output, isolation, approval, sandbox, artifacts | capability별 native 구현이 계약·권한·취소·오류 검증 통과 |
| Codex 필수 의존 제거 | bootstrap, resume, configuration, session/state persistence 등 남은 의존 | Codex 바이너리·app-server 없이 새 작업과 상태 복구를 끝까지 수행 |

모델 실행과 tool/runtime 기능은 검증된 capability부터 대체한다. adapter 뒤에서 legacy/native 구현을 선택할 수 있게 하여 기능별 비교와 rollback을 가능하게 한다. 미구현 capability를 조용히 비격리 실행하거나 승인 없이 우회하지 않는다.

Runtime Adapter는 model session 실행과 exec/tool/isolation/approval 등 서로 다른 capability를 구분한다. Codex의 한 API가 모두 제공한다고 가정하지 않는다. 각 교체 시 기존 권한 경계, 승인 대기·응답, process 취소, output streaming, repository 격리, 오류·재접속 동작을 검증한다.

완전 교체 이후 Codex 호환 backend를 선택적으로 남길 수 있지만, 기본 제품 실행과 복구에는 필요하지 않아야 한다. ‘runtime 재사용’은 개발 전략이며 최종 제품 정체성이 아니다.

## 16. 제외 항목과 다음 구현 단위

처음부터 하지 않을 것: 네 모델 broadcast, worker 직접 대화, agent society, 매 턴 자동 요약, 모든 정보 vector DB화, FLAGSHIP speculation, 모델 merge 권한, session을 Source of Truth로 사용, custom runtime 일괄 재작성, 학습 router 우선 개발.

현재 구현한 단위는 **MVP 1의 event/command 경계**, 비동기 LOCAL 공유 탐색, 작업 중 애니메이션, 질문 선택·직접 입력 경계, ActivityTask projection과 LOCAL/Mock 활동 상세다. [TUI 완성 계획](tui-completion-plan.md)에 화면 기능과 순서를 기록한다. UI는 provider JSON을 해석하지 않고, 승인 payload도 adapter가 소유한다. 기존 JSON fixture는 테스트에서만 adapter를 거쳐 재사용한다. Mock Core·로컬 WebSocket fixture·PTY로 현재 UI 흐름과 요청 경계를 검증한다.

공유 코드 탐색은 네 동시 소비자가 같은 index와 query result를 공유하는 검증을 포함한다. 아직 네 실제 원격 모델을 동시에 실행하거나 patch 충돌을 해결한다는 뜻은 아니다. 다음 구현 단위는 MVP 2의 durable CanonicalState·TaskGraph·WorkerTask/Result와 이 공유 자원의 연결이다.

## 17. 후속 요구: 기본 효율 기능

외부 제품을 복제하거나 사용자가 스킬을 호출해야 동작하는 방식으로 붙이지 않는다. 다음 기능의 책임을 자체 Core에 기본으로 통합한다. Caveman의 문체 압축은 기본 기능으로 채택하지 않는다.

- 공유 Repository Explorer: LOCAL/SMALL/MEDIUM/FLAGSHIP이 하나의 revision별 인덱스와 evidence cache를 사용한다. 읽기는 불변 snapshot에서 병렬 실행하고 동일 탐색은 합친다. 수정 충돌은 별도의 patch isolation·Core merge로 처리한다.
- 코드 관계 검색: 로컬 parsing으로 추출 가능한 symbol/import/call 관계부터 도입한다. 추출된 사실과 추정 관계를 구분하고 파일 변경으로 무효화한다. [Graphify](https://github.com/Graphify-Labs/graphify)는 기능 참고 자료이며 필수 gateway나 자동 실행 스킬로 설치하지 않는다.
- 자체 retrieval/context compilation: 필요한 증거를 budget 안에 투영하고 원문은 artifact ref로 남긴다. worker가 이미 받은 유효한 evidence를 중복 주입하지 않되, 새 session/epoch에는 필요한 사실을 다시 제공한다.
- 자동 context 이어가기: durable goal/task/decision/failure/evidence에서 재구성한다. session/epoch와 repo revision을 함께 검증하며, 매 턴 요약이나 전체 conversation 복사에 의존하지 않는다.
- 출력 압축: test/search/git 결과에 deterministic 구조화를 우선한다. 원문·절단 여부·검증 근거를 보존하며 실제 remote token/cost로 효과를 확인한다.
- 무료 provider 자원: OpenRouter의 동적 model catalog에서 가격·context·필요 capability·가용성을 확인한 뒤 Core가 후보를 선택한다. quota/rate limit/실패에는 bounded retry와 cooldown을 적용한다. 유료 fallback은 설정된 budget·정책을 따른다. 어떤 실제 모델을 호출했는지도 기록한다.

사용자가 제시한 [OmniRoute 소개](https://news.hada.io/topic?id=31710)는 resource pooling·fallback의 참고 자료다. 게이트웨이 전체나 압축 스택을 제품 기본 의존성으로 복사하지 않는다. [OpenRouter 공식 설명](https://openrouter.ai/docs/cookbook/get-started/free-models-router-playground)의 `openrouter/free`는 무료 모델 선택을 OpenRouter가 수행하므로, 자체 Core의 capability·cache affinity·품질 정책을 보장하는 선택과 구분한다. native provider 연결과 routing은 Task/Worker/State 계약 이후 구현한다.
