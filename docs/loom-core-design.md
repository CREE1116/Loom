# Loom Core 설계안 (bottom-up)

상태: 설계 기준 · 2026-10-02. [구현 기획서](orchestrator-implementation-plan.md)의 목표와 원칙, [실험 결과](loom-experiment-findings.md)의 결론을 구현 단위로 내린 문서다. 기획서가 "무엇을"이면 이 문서는 "어떤 순서로, 어떤 경계로, 어떻게 검증하며"다.

> Core는 세계를 기억하고, Worker는 문제를 푼다.

구현 진행 (2026-10-02): 별도 [`loom-core`](../loom-core/README.md) crate에
L0–L7의 단일 Worker 경로, Native backend와 CLI를 구현했다. raw revision·격리·검증·
승인·충돌 보존·중단 재개·재시도 인계·실행 lease·단조 승격을 연결했고, L2 warm build는 raw 복원 경로에서
0.90%로 gate를 통과했다. 실모델 pilot은 시간 초과로 Native 0/12·수락 0건이며
L4 baseline 품질 gate와 L5 원격 token 효율 gate를 통과했다고 주장하지 않는다.
당시 Context Compiler는 opt-in, Split은 기록만 하고 Single로 실행했다. scheduler는
측정으로 정당화되기 전까지 보류했다. 2026-10-04 사용자 요구에 따라 제한된
기존 코드 범위의 자동 배분을 연결했다. [실행 감사](loom-agent-session-audit-2026-10-04.md)에
적용 범위와 미완성 기획기, 실모델 실패를 명시한다. 조건과 제한은 crate README와
[측정 기록](../loom-core/measurements/2026-10-02.json)에 남겼다.

## 0. 왜 다시 설계하나

지금까지 Core 기능은 `custom-tui/src/core` 안에 TUI와 섞여 자랐고, 자기개선 루프가 roadmap 항목을 하나씩 붙이는 방식이었다. 결과:

- 계층이 없다. `tools.rs`(1.2k줄)가 파일 I/O, 검색, 편집, 빌드 실행, 출력 가공을 다 한다. Worker가 사용자 작업 폴더를 직접 고친다. Core가 "소유"하는 상태가 없다.
- 기능이 서로를 전제하는데 순서가 없다. Effect Contract 없이 Scope Monitor를, 격리 없이 병렬을, Core 검증 없이 escalation을 만들려 했다.
- 루프는 작은 개선에는 맞지만, 경계를 새로 긋는 일에는 맞지 않는다. 약한 모델이 큰 기능을 한 번에 만들다 되돌려지기를 반복했다.

그래서 Core를 **별도 crate로, 아래 계층부터** 만든다. 각 계층은 위 계층 없이 단독으로 테스트되고, 통과 기준(gate)을 넘어야 위 계층을 시작한다.

## 1. 설계 원칙

기획서 §2와 실험 §4를 구현 규칙으로 옮긴 것이다.

1. **계층은 아래만 안다.** L(n)은 L(<n)의 공개 타입만 쓴다. 위로 가는 의존은 trait과 이벤트로만 한다.
2. **상태는 Core가 소유한다.** Worker는 계산 자원이다. Worker 결과는 Core가 검증하고 적용할 *제안*(patch, finding)이다.
3. **사용자 작업 폴더는 Core만 쓴다.** Worker는 격리된 작업 사본에서 일하고 patch를 낸다. 단일 Worker에서도 같다. 그래야 검증, 되돌리기, 병렬, escalation이 같은 경로를 탄다.
4. **모든 출력은 구조화 + 원문 ref.** 모델에는 요약을, 원문은 artifact로 남기고 ref로 가리킨다.
5. **이산 상태로만 판단한다.** 성공/실패/승격/분할은 열거형 상태와 관측값으로 결정한다. 모델의 자기평가("다 했다")는 증거가 아니다.
6. **측정 없이는 채택하지 않는다.** 각 계층은 단계별 비용(Orient/Edit/Validate) telemetry를 낸다. 효율 기능은 bench 수치로 채택한다.
7. **기본은 단일 Worker.** 분할은 Split Gate가 관측값으로 이득을 증명할 때만 한다.
8. **유료 모델 차단은 Core 상수다** (`COST_CEILING`). 설정으로 풀 수 없다.

## 2. 구조

```text
loom-core/                (새 crate, UI 없음)
  L0 base        ids · Revision · ArtifactStore · EventLog · Cost/Phase 타입
  L1 world       Snapshot(revision) · RepoIndex · CodeGraph · 무효화
  L2 effects     읽기/검색/편집 primitive · Runner(build/test) · Overlay(격리 사본) · Patch
  L3 models      Provider adapters · Router · Ledger · FreeTier          (기존 코드 이관)
  L4 worker      WorkerTask → WorkerResult 실행 루프 (단일 모델, 단일 overlay)
  L5 context     EffectContract · Context Compiler(slice) · Scope Monitor
  L6 control     Task/TaskGraph · CanonicalState · 검증 · patch 적용 · escalation · Split Gate · scheduler
  L7 surfaces    NativeBackend(AgentBackend 구현) · `loom` CLI · bench harness

custom-tui/               UI만. AgentEvent/AgentCommand로 L7 NativeBackend 또는 Codex adapter를 구동
```

Codex adapter(`custom-tui/src/backend/codex.rs`)는 유지한다. NativeBackend가 같은 `AgentBackend` trait을 구현하므로 UI에서 backend를 선택할 수 있고, capability별로 비교와 rollback이 가능하다 (기획서 §15).

## 3. 계층별 설계

각 계층마다: 책임 · 핵심 타입 · 기존 코드에서 가져올 것 · gate(통과 기준).

### L0 base: 공통 어휘와 기록

책임: 모든 계층이 쓰는 식별자, revision, 원문 저장, 이벤트 기록.

```rust
pub struct Revision(String);            // git tree hash (작업 폴더 상태 포함, 커밋 불필요)
pub struct ArtifactRef { id: String, bytes: u64 }   // 내용 해시
pub trait ArtifactStore { fn put(&self, bytes: &[u8]) -> ArtifactRef; fn get(&self, r: &ArtifactRef, range: Option<Range<usize>>) -> Vec<u8>; }
pub enum Phase { Orient, Edit, Validate, Message }
pub struct Cost { input: u64, cached: u64, output: u64, usd: Option<f64>, ms: u64 }
pub struct EventLog;                    // append-only JSONL, 계층별 이벤트 직렬화
```

- 저장 위치: `<workspace>/.loom/` (artifacts/, events.jsonl). 사용자 저장소의 index와 HEAD는 건드리지 않는다.
- 가져올 것: `telemetry.rs`의 Phase와 PhaseLedger, `checkpoints.rs`의 git tree 기록 방식.
- **gate:** revision이 같은 작업 폴더에서 결정적(같은 내용 → 같은 hash, 파일 하나 바꾸면 다른 hash). artifact put/get/range 왕복. 이벤트 로그 재생으로 같은 상태 복원. unit test만으로 판정.

### L1 world: 불변 snapshot 위의 코드 지식

책임: revision 하나에 대한 읽기 전용 세계. 파일 목록, 검색 index, 코드 graph. revision이 바뀌면 바뀐 파일만 무효화한다.

```rust
pub struct Snapshot { revision: Revision, files: Arc<FileTable> }
pub struct RepoIndex;   // find(query) -> Vec<Evidence{path, line, excerpt, file_digest}>, 중복 질의 합침
pub struct CodeGraph;   // symbols, symbol_at, resolve, referrers, neighbors(id), signature(id)
pub struct World { snapshot: Snapshot, index: Arc<RepoIndex>, graph: Arc<CodeGraph> }
impl World { fn advance(&self, changed: &[Path]) -> World; }   // 증분 갱신
```

- Symbol id 규칙은 실험 때 정한 `<file>::<item path>`를 유지한다. 필드/variant 단위 id(`Opts.field`)를 추가한다 (실험 §5.1).
- 가져올 것: `repository.rs`(RepositoryExplorer, Evidence), `graph.rs`(CodeGraph, GraphIndex). symgraph와의 parity 4827/4827은 회귀 테스트로 고정한다.
- **gate:** (1) 파일 하나 수정 후 `advance`가 그 파일의 symbol만 다시 파싱 (parse 횟수로 검증). (2) 동시 질의 4개가 index를 공유 (기존 공유 탐색 테스트). (3) ripgrep 저장소에서 graph 생성 시간과 메모리 기록 (baseline).

### L2 effects: 세계를 바꾸거나 관측하는 primitive

책임: 모델과 무관한 결정적 동작. 모든 동작은 `Output{phase, summary, structured, artifact}`를 낸다.

```rust
pub struct Overlay { base: Revision, dir: PathBuf }    // git worktree 또는 copy-on-write 사본
impl Overlay {
    fn read(&self, path, range) -> Output;              // 큰 파일은 outline으로
    fn edit(&mut self, path, old, new) -> Output;       // fuzzy 매칭 + syn 구문 검사 + read-before-edit
    fn write(&mut self, path, content) -> Output;
    fn rename(&mut self, symbol, new_name, world: &World) -> Output;   // LOCAL 기계적 수정
    fn touched(&self) -> BTreeSet<PathBuf>;             // 실측 수정 파일
    fn patch(&self) -> Patch;                           // base 대비 diff
}
pub struct Runner;      // build/test 실행, 취소, 시간 제한 → TestResult
pub struct TestResult { passed: u32, failed: u32, failing: Vec<String>, first_error: Option<String>, artifact: ArtifactRef }
pub struct Patch { base: Revision, files: Vec<FilePatch> }
```

- **Overlay가 핵심이다.** Worker는 항상 Overlay에서 일한다. Overlay 생성 비용이 작아야 한다: git worktree + 공유 target dir (revision별 증분 빌드, 실험 §5.5 "빌드" 행).
- 가져올 것: `tools.rs`의 편집 매칭, 구문 검사, outline, check 구조화 출력, read_artifact. 단, Workspace 하나에 몰린 책임을 Overlay / Runner / ArtifactStore로 나눈다.
- **gate:** (1) Overlay 편집이 원본 작업 폴더를 바꾸지 않음. (2) `patch()`를 원본에 적용하면 Overlay와 같은 tree. (3) Runner 취소 시 자식 프로세스까지 종료 (프로세스 그룹). (4) Overlay 생성+첫 빌드 시간 측정, 두 번째 Overlay는 증분 빌드로 첫 빌드의 20% 이내.

### L3 models: 모델 호출 (이관)

책임: provider 프로토콜, 라우팅, 사용량 원장, 무료 등급. 지금 코드가 가장 완성돼 있는 계층이다.

```rust
pub struct Need { min_tier: Tier, tools: bool, max_cost: CostClass }
pub trait ModelPort { fn complete(&self, need: Need, req: &Request) -> Result<Served>; }  // L4가 쓰는 유일한 입구
```

- 가져올 것: `core/model/*` 전부 (chat/responses/anthropic/ollama adapter, router, discover, local, gateway, freetier, ledger).
- 추가: 테스트용 `ScriptedModel`(정해진 응답 순서). L4 이상의 모든 테스트가 이걸로 돈다. 실제 모델 없이 결정적으로.
- 추가: 경로 금지 목록 (`route ∈ {cc, antigravity, …}`이면 응답 거부 + cooldown). 구독 OAuth 경로 방지 2차 장치.
- **gate:** 기존 router 테스트 전부 + ScriptedModel로 tool-call 왕복 + `COST_CEILING` 우회 불가 테스트.

### L4 worker: 한 모델이 한 Overlay에서 일하는 루프

책임: `WorkerTask`를 받아 모델과 L2 primitive로 문제를 풀고 `WorkerResult`를 낸다. 결정하지 않는다. 적용하지 않는다.

```rust
pub struct WorkerTask {
    goal: String,
    overlay: Overlay,
    slice: Option<Slice>,          // L5가 채움. 없으면 빈 맥락으로 시작
    handoff: Option<Handoff>,      // 하위 tier의 실패 evidence (L6 escalation)
    tools: ToolSet,                // read-only / edit 권한
    budget: Budget { steps, seconds, tokens },
    need: Need,
}
pub enum WorkerStatus { Proposed, Unresolved, Failed(String) }   // "done"은 없다. 완료 판정은 L6
pub struct WorkerResult { status, patch: Patch, touched, summary, rereads: u32, cost_by_phase: BTreeMap<Phase, Cost>, trace: ArtifactRef }
```

- `finish`는 "제안 완료"다. Worker는 자기 결과가 맞다고 선언할 수 없다.
- 가져올 것: `worker.rs` 루프(도구 호출 처리, 무응답 nudge, trace). Workspace 대신 Overlay를 받도록 바꾼다.
- **gate:** (1) ScriptedModel로 수정→제안 흐름. (2) budget 초과 시 Unresolved. (3) 엔진 bench 12개를 L4 단독(+L3 무료 모델)으로 돌려 **현재 엔진과 같은 통과 수**. 이게 새 Core의 출발점 baseline이다.

### L5 context: 계약과 맥락

책임: 작업을 시작하기 전에 결정적으로 범위를 정하고(Effect Contract), 그 범위의 코드를 모델에 미리 준다(slice). 끝난 뒤 실제 범위와 비교한다(Scope Monitor).

```rust
pub struct EffectContract {
    anchor: Vec<SymbolId>, write_files: Vec<PathBuf>, write_hints: Vec<SymbolId>,
    test_slots: Vec<TestSlot>, artifacts: Vec<ArtifactClass>, reads: Vec<SymbolId>,
    provenance: Provenance { derived_by: Rules | Model | Both, revision: u32 },
}
pub fn derive(goal: &str, world: &World) -> EffectContract;            // 모델 호출 없음 (실험 §5.2 ①③)
pub fn compile(contract: &EffectContract, world: &World, cap: usize) -> Slice;  // anchor 본문 + hint/read signature
pub fn monitor(contract: &EffectContract, touched: &BTreeSet<PathBuf>) -> ScopeReport { events, recall, revised }
```

- 모델이 필요한 경우(anchor가 모호)만 SMALL tier에 anchor 확정을 묻는다 (실험 §5.2 ②). 출력은 symbol id 몇 개다.
- slice 품질은 `rereads`(이미 준 symbol을 다시 읽은 횟수)로 측정한다.
- 가져올 것: `tools.rs::find_anchor_candidates`(Context Compiler v0), graph의 referrers/resolve.
- **gate = 실험 §7의 남은 검증.** bench에서 L4 단독 대비 **원격 input token 50% 이상 감소, 통과 하락 2건 이내**. 실패하면 효율 주장을 축소하고 L6의 분할 관련 부분(Split Gate, scheduler)을 보류한다.

### L6 control: Core가 결정하고 적용한다

책임: 사용자 요청 하나의 수명. 상태 소유, 검증, 적용, 재시도, 승격, 분할.

```rust
pub struct Task { id, goal, status: TaskStatus, contract, mode: ExecutionMode, attempts: Vec<Attempt>, decision: Option<SplitDecision> }
pub enum TaskStatus { Planned, Running, Validating, Accepted, Unresolved, ValidationFailed, ScopeConflict, Cancelled }
pub struct CanonicalState { revision: Revision, tasks: TaskGraph, findings: Vec<Finding>, failures: Vec<Failure> }  // EventLog 재생으로 복원
```

처리 순서 (단일 Worker):

```text
요청 ─▶ derive contract ─▶ compile slice ─▶ Overlay 생성 ─▶ L4 Worker(최저 tier)
     ─▶ Validate(Runner, 영향받는 테스트) ─┬─ 통과 ─▶ Scope Monitor ─▶ patch 적용(base 확인) ─▶ Accepted
                                           └─ 실패 ─▶ 같은 tier 1회 재시도(TestResult 전달)
                                                  ─▶ 다음 tier로 승격(Handoff: contract, slice, patch, 실패 증거)
```

- **검증은 Core가 한 번.** Worker의 "다 했다"는 Validate를 통과해야 Accepted.
- **승격은 이산 상태에서만**: Unresolved / ValidationFailed / ScopeConflict. 단조 증가 Local→Small→Medium→Flagship, `max_cost` 안에서.
- **적용**: patch의 base revision이 현재 작업 폴더와 다르면(사용자가 그 사이 수정) 3-way 적용, 충돌이면 사용자 변경 보존하고 ScopeConflict.
- **Split Gate**: `observed_fixed_cost`(EventLog의 과거 Orient+Validate 비용 중앙값)와 `est_work`(contract 규모)로 판정, 이유와 추정치를 기록. 처음에는 기록만 하고 Single로 실행한 뒤 예측 오차를 잰다.
- **scheduler**: Split이 정당화된 뒤에만. 단위마다 Overlay, lease로 write_files 겹침 차단, 충돌 시 직렬화, Mergeable artifact는 Core가 병합.
- **gate (단계별):** (a) 단일 Worker 수명 전체를 ScriptedModel로 재현, 중간에 프로세스를 죽였다 살려도 EventLog로 이어감. (b) bench `false_finish = 0` (Core 검증 덕분에 "완료"인데 숨은 테스트 실패가 없어야 함). (c) escalation: 무료 tier만으로 못 푼 과제 중 상위 tier로 승격돼 풀린 비율과 비용 기록. (d) Split: 다중 기능 과제 세트에서 Split 판정의 예측 시간 오차 측정 후 채택 여부 결정.

### L7 surfaces: 바깥과 연결

- `NativeBackend: AgentBackend` — L6 이벤트를 `AgentEvent`로 투영한다. Task, Attempt, tier, 비용이 Activity 패널에 그대로 나온다. 승인/중단은 `AgentCommand`로 받는다.
- `loom run "goal"` CLI — 지금의 `custom-tui --run`을 대체한다. bench와 탐색 테스터가 쓰는 입구.
- bench harness — `scripts/loom-enginebench`가 `loom run`을 호출하고 계층별 지표(통과, token, phase별 bytes, rereads, recall, false_finish, escalated)를 집계한다.
- **gate:** TUI에서 backend를 Native로 바꿔 실제 저장소에서 수정 요청 → 승인 → 완료까지. 기존 PTY suite가 Native backend에서도 통과.

## 4. 기존 코드 처리

| 현재 위치 | 새 위치 | 처리 |
| --- | --- | --- |
| `core/model/*` | L3 | 그대로 이관, ScriptedModel과 경로 금지 목록 추가 |
| `core/graph.rs`, `core/repository.rs` | L1 | 이관, `World::advance` 증분 갱신 추가 |
| `core/telemetry.rs` | L0 | Phase/Cost는 L0, 명령 분류(`classify_command`)는 L2 Runner |
| `core/checkpoints.rs` | L0/L2 | tree 기록은 Revision, 되돌리기는 Overlay/patch로 대체 |
| `core/tools.rs` | L2 + L4 | primitive는 Overlay/Runner로, tool spec과 모델용 문구는 L4 ToolSet으로 분리 |
| `core/worker.rs` | L4 | Overlay 기반으로 수정 |
| `core/backend.rs`, `core/turns.rs` | L7 | Codex 과도기 연결. NativeBackend가 생기면 Codex adapter 쪽으로 정리 |

이관은 계층 단위로 한다. 한 계층을 옮길 때 `custom-tui`는 `loom-core`를 re-export해서 빌드를 유지한다. 옮긴 계층의 테스트가 새 crate에서 통과하면 옛 파일을 지운다.

## 5. 만드는 순서와 기간 감각

| 단계 | 내용 | 완료 증거 |
| --- | --- | --- |
| 1 | crate 생성, L0 | revision/artifact/event 재생 테스트 |
| 2 | L1 이관 + 증분 갱신 | parity 4827/4827, 증분 parse 테스트 |
| 3 | L3 이관 + ScriptedModel | 기존 router 테스트 + scripted tool-call |
| 4 | L2 Overlay/Runner/Patch | 격리·patch 왕복·취소·증분 빌드 측정 |
| 5 | L4 Worker on Overlay | **bench baseline: 현재 엔진과 같은 통과 수** |
| 6 | L6 단일 Worker 수명 (검증·적용·EventLog 복구) | false_finish 0, 중단 후 재개 |
| 7 | L5 Contract + slice + Scope Monitor | **§7 검증: 원격 input −50%, 통과 하락 ≤2** |
| 8 | L6 escalation | 승격으로 추가로 풀린 과제 수와 비용 |
| 9 | L7 NativeBackend + `loom run` | TUI에서 Native로 실제 작업 완료, PTY suite 통과 |
| 10 | Split Gate 기록 → 오차 측정 → (이득 시) scheduler | 예측 오차와 실측 시간 비교 |

L3을 L2보다 먼저 두는 이유: 이관만 하면 되고, ScriptedModel이 L2·L4 테스트에 필요하다. L6 단일 수명을 L5보다 먼저 두는 이유: Core 검증과 적용 경로가 있어야 L5 효과(token 감소가 품질을 깎지 않는지)를 false_finish와 함께 판정할 수 있다.

## 6. 개발 방식

- **계층 구현은 사람(또는 강한 모델과의 대화형 세션)이 한다.** 단계마다 이 문서의 타입과 gate를 기준으로 PR 하나. gate 테스트를 먼저 쓰고 구현한다.
- **자기개선 루프는 완성된 계층의 튜닝에만 쓴다.** 예: L4 도구 문구와 출력 크기 조정(engine target), L5 slice 깊이와 cap 조정. 판정은 해당 계층의 bench 지표. 계층 경계를 바꾸는 일은 루프에 맡기지 않는다.
- **탐색 테스터는 L7 이후 실사용 경로를 친다.** `loom run`과 TUI Native backend가 대상이다.
- **bench를 함께 키운다.** 지금 12개 과제는 작은 단일 파일 위주다. L5·L6 gate에는 여러 파일, 원인이 먼 버그, 새 테스트가 필요한 과제가 필요하다. 실험 때 쓴 실제 PR(ripgrep, fd, just) 일부를 과제화한다.

## 7. 정하지 않은 것

- Overlay 구현: git worktree(단순, 디스크 사용) vs 파일 단위 copy-on-write(빠름, 직접 구현). 단계 4에서 생성 시간과 빌드 공유를 재보고 정한다.
- CanonicalState 저장: JSONL 이벤트 재생으로 시작한다. 재생 시간이 문제가 되면 snapshot을 추가한다. DB는 필요가 측정될 때.
- 비 Rust 저장소: L1 graph는 Rust(syn) 전용이다. 다른 언어는 tree-sitter를 쓰거나 파일 단위 계약으로 낮춘다(원칙 3: W는 파일이 권위).
- 지속 원격 세션과 prompt cache 안정 prefix (기획서 MVP 4): L4 gate 이후 cache 지표를 보고 넣는다.
