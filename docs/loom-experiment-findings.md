# LOOM 실험 결과와 아키텍처 방향

상태: 2026-10-02. effect-contract 실험(`experiments/effect-contract/`)의 결론과 그에 따른 설계 수정안이다. [구현 기획서](orchestrator-implementation-plan.md)와 충돌하는 부분은 §6에 정리했고, 기획서 반영은 별도 결정으로 남긴다.

> 분할은 유효하다. 그러나 시간 단축 수단은 아니다. 그래프는 작업을 나누는 도구가 아니라 작업 경계를 아는 도구다.

## 1. 실험이 답하려던 질문

실제 소프트웨어 변경 작업은 정확한 read/write effect와 dependency graph로 표현했을 때, 의미 있는 독립 실행 단위로 분해되는가? 분해된다면 그 분해가 실제로 빠르거나 싼가?

환경:

| 항목 | 값 |
| --- | --- |
| Corpus | ripgrep · fd · just. git history의 change commit 2246개, merge된 PR 987개 |
| Symbol 단위 | Rust item (`<file>::<item path>`). syn 기반 추출기 `symgraph` |
| R 해석 | identifier 이름 기반 해석 (과대 근사). method는 타입명이 함께 등장할 때만 연결 |
| 모델 | gpt-web bridge `chatgpt-web/gpt-5.6-sol` (codex exec, 사용자 설정 미적용) |

## 2. 결과

### 2.1 존립성: 실제 변경은 독립 단위로 쪼개지는가 (Exp2, oracle, 1399 commit)

task를 "바뀐 symbol 하나"로 두고 `W_i ∩ R_j ≠ ∅`이면 i→j 간선을 둔 뒤 SCC로 축약했다.

| 기준 | WorkerUnit / task | 하나로 붕괴 | 병렬 2 이상 | critical path / task |
| --- | --- | --- | --- | --- |
| 직접 참조 | 5 / 5 | 0% | 91% | 0.40 |
| 테스트 제외 + 전이적 참조 | 3 / 5 | 12% | 60% | 0.50 |
| 파일 단위 | 2 / 3 | 7% | 64% | 0.50 |

**쪼개진다.** 거대 SCC로 붕괴하는 경우는 드물다. 다만 병렬 폭은 2~4로 좁다.

맥락 중복(Exp2b): unit을 모두 따로 실행하면 같은 코드를 평균 1.44배 읽는다. 균형 잡힌 2-Worker로 묶으면 1.10배다. 동시에 실행되는 unit끼리의 맥락 Jaccard 중앙값은 0.13이다.

### 2.2 안전성: 독립 판정은 믿을 만한가 (Exp3, 연속 PR 115쌍)

oracle patch의 순서를 바꿔 적용(B→A)하고, 예측과 관측을 비교했다.

| effect universe | 독립 예측 중 실제 비가환 (unsound) | 실제 가환 중 의존으로 판정 (과잉 보수) |
| --- | --- | --- |
| Rust symbol만 | 4 / 66 | 36 / 98 |
| symbol + non-Rust 파일 | **0 / 61** | 37 / 98 |
| 전이적 R | 2 / 33 | 67 / 98 |

- symbol-only 판정이 틀린 4건은 모두 `CHANGELOG.md`, `Cargo.toml`, man page 충돌이었다. 이 파일들을 W에 넣으면 unsound가 0이 된다.
- 과잉 보수의 주원인은 `Opts`, `Config` 같은 대형 struct를 symbol 하나로 다루는 것이다.
- 전이적 R은 sound 측면의 이득 없이 병렬성만 절반으로 줄였다.
- 이 실험은 텍스트 수준 가환성만 검증했다. 행동 수준의 비가환성은 다루지 않았다.

### 2.3 계약: Planner가 미리 선언할 수 있는가 (Exp1, PR 38건)

Planner(read-only)가 anchor, R, W를 선언했다. Executor는 계약을 보지 않고 별도 세션에서 실제 수정을 했다.

| 비교 | W recall | 누락 0인 task |
| --- | --- | --- |
| 선언 vs merge된 PR | 0.82 (중앙값 1.0) | 22/38 |
| 선언 vs executor 실제 수정 | 0.57 | 7/37 |

executor 기준 누락 92건의 내역: 새 테스트 54, 기존 테스트 14, 기존 코드 22, 새 코드 2. 선언하지 않은 *파일*을 수정한 것은 8건(3 task)이었고, 그중 5건은 범위가 열린 lint 정리 task였다.

**선언 밖 수정 자체는 문제가 아니다 (Exp1b).** 선언 W를 그래프로 분할한 뒤 실제 수정을 unit에 배정해 보면, 동시 실행 unit의 W∪R을 침범한 것은 실제 수정 184건 중 1건(1/38 task)이다. 문제는 균형이다. 병렬 분할 32건 중 7건은 실제 작업이 한 unit에 100% 몰렸다. 유용한 분할(병렬 2 이상, 가벼운 쪽 ≥30%, 침범 없음)은 16/38이다.

### 2.4 효율: 실제로 빠른가 (Exp6, End-to-End, 38건)

- A: 단일 agent.
- B: Planner 계약 → 결정론적 2-Worker 분할(서로 의존 없는 덩어리, 가벼운 쪽 ≥25%) → 병렬 Worker → 3-way merge. 분할이 안 나오는 task는 단일 실행에 Planner 시간을 더했다.
- 판정 규칙(사전 고정): wall B/A ≤ 0.8 이고 성공 하락 ≤ 2.

| | A 단일 | B LOOM-lite |
| --- | --- | --- |
| hidden test 성공 (유효 15건) | 9 | 10 |
| build 성공 | 34/38 | 34/38 |
| wall-clock | 166분 | 252분 (1.52배) |
| input token | 31.9M | 50.5M (1.58배) |

**NOT EFFICIENT.** 분할된 12건만 보면:
- 느린 Worker 하나만으로도 A의 1.17배다 (Planner 제외).
- 작업 절반을 맡은 Worker 하나가 A token의 75%를 썼다.
- 병합 충돌은 1/12다.

agent 시간과 비용의 대부분은 편집량이 아니라 **고정비**다: repo 파악, 빌드, 테스트. PR 규모(중앙값 수십 줄)에서는 나누면 고정비만 두 번 낸다.

### 2.5 비용 구성: 원격 모델은 무엇에 쓰이는가 (Exp1·Exp6 trace 62 run)

executor가 맥락에 넣은 텍스트를 단계별로 나눴다. 측정 단위는 명령 출력과 메시지의 문자 수다. 원격 input token은 이 맥락을 매 턴 다시 보내면서 쌓이므로, 비율이 대체로 비례한다.

| 단계 | 내용 | 비중 (전체) | run별 중앙값 |
| --- | --- | --- | --- |
| Orient | rg, sed, cat, ls 등 탐색 | 61% | 69% |
| Validate | cargo build/test/check, git diff | 37% | 27% |
| Edit | 실제 파일 수정 | 1% | 1% |

원격 모델이 받는 맥락의 대부분은 결정론적으로 처리할 수 있는 탐색과 검증 출력이다. 이 부분을 LOCAL(그래프 slice, Core 검증, 출력 구조화)로 옮기면, 편집 판단에는 손대지 않고도 원격 비용의 대부분을 줄일 수 있다. 품질에 직접 영향을 주는 Edit 단계는 원격 모델이 그대로 맡는다.

## 3. 결론

| 가설 | 판정 |
| --- | --- |
| 실제 변경은 독립 단위로 분해된다 | 확인 (폭은 2~4) |
| R/W 기반 독립 판정은 안전하다 | 텍스트 수준에서 확인. 단 non-Rust artifact 포함이 필수 |
| Planner가 사전에 effect를 선언할 수 있다 | 파일 단위로는 확인. symbol 단위는 인접 symbol·새 테스트를 놓침 |
| 분할하면 빨라진다 | **반증** (PR 규모, 독립 agent형 Worker 기준) |
| 분할하면 싸진다 | 같은 모델 기준 반증. tier routing은 미검증 |
| 원격 비용의 대부분은 LOCAL로 옮길 수 있는 작업이다 | 지지 (탐색 + 검증 = 맥락의 98%) |

설계상 가장 중요한 교훈: **"쪼갤 수 있는가"와 "쪼개야 하는가"는 다른 질문이다.** 판정이 정확해도, 이득 없는 분할은 손해다. Core의 책임은 이 착각을 막는 것이다.

## 4. 설계 원칙 (수정)

1. **기본 실행은 단일 Worker다.** 분할은 이득이 측정 근거로 성립할 때만 하는 예외다.
2. **그래프는 경계를 아는 도구다.** 범위 감시, 충돌 감지, 검증 범위 결정, 맥락 slicing에 쓴다. 분할은 그 용도 중 하나일 뿐이다.
3. **W는 파일이 권위, symbol은 힌트다.** 계약 위반 판정은 파일 단위로 하고, symbol은 병렬 판정과 맥락 slicing에만 쓴다.
4. **non-Rust artifact는 W의 1급 원소다.** CHANGELOG처럼 append 병합이 가능한 artifact는 별도 클래스로 둔다.
5. **고정비를 줄이는 것이 1순위 최적화 대상이다. 수단은 LOCAL 이관이다.** 병렬화보다 우선한다. Worker가 매번 repo를 처음부터 파악하고 따로 빌드하는 구조에서는 어떤 orchestration도 이득을 내기 어렵다.
6. **판정 기준값은 튜닝하지 않고 관측한다.** Worker 고정비, 작업량 등은 Core가 trace에서 측정한 값을 쓴다 (parameter-free).

## 5. 구체 설계안

### 5.1 Effect Contract

```text
EffectContract {
  anchor:      [SymbolId]               // 1-3, 변경 중심
  write_files: [Path]                   // 권위. 위반 판정 기준
  write_hints: [SymbolId]               // 병렬 판정·slicing용
  test_slots:  [{ file|module, kind: Add|Modify }]   // 새 테스트는 이름 대신 위치
  artifacts:   [{ path, class: Mergeable|Exclusive }] // CHANGELOG=Mergeable, Cargo.toml=Exclusive
  reads:       [SymbolId]               // 직접 참조. 스케줄링용
  provenance:  { derived_by: Rules|Model|Both, revision }
}
```

- `write_hints`의 대형 struct는 필드/variant 단위 id(`Opts.field`)를 허용한다. Exp3에서 과잉 보수의 주원인이었다.
- 전이적 reads는 계약에 넣지 않는다. 검증 범위 계산(§5.5)에서만 그때그때 계산한다.

### 5.2 계약 도출 파이프라인

```text
issue ─▶ ① anchor 후보 (LOCAL retrieval: ubis find/near) ─▶ ② (필요 시) MODEL이 anchor 확정
      ─▶ ③ 결정론적 확장: anchor ∪ 같은 파일 인접 symbol ∪ 직접 호출자 ∪ test_slots ∪ artifacts
      ─▶ EffectContract
```

- 의미 해석이 필요한 단계는 ②뿐이다. 모델 출력은 anchor 몇 개로 짧아진다. Exp1 Planner는 task당 input 26만 token, 1.7분을 썼다.
- ③은 Exp1의 실패 패턴(인접 symbol, 새 테스트, non-Rust 파일)을 규칙으로 덮는다.

### 5.3 실행 모드와 Split Gate

```text
ExecutionMode = Single | Split{ workers: [WorkerUnit] }

split  iff  ∀ 조각 p:  est_work(p) > observed_fixed_cost(repo, tier)
        and 조각 간 cross dependency 없음 (unit DAG의 weakly connected component 단위)
        and 조각 간 맥락 중복이 작음 (R 겹침 최소 packing)
```

- `observed_fixed_cost`는 trace에서 관측한다: 편집 0줄에 가까운 실행의 시간과 token, 즉 repo 파악과 검증 비용. 이번 데이터에서는 2~4분, input 20만 token 이상이었다.
- `est_work`는 anchor symbol 크기, 확장된 write_hints 규모, co-change 이력의 과거 변경량으로 추정한다. 추정이 불확실하면 Single이다.
- Split 결정은 항상 이유와 함께 기록한다 (`SplitDecision { mode, reason, estimates }`). 사후에 실제 시간과 비교해 판정 모델의 오차를 측정한다.

### 5.4 범위 감시 (Scope Monitor)

- **Single:** 선언 밖 수정은 허용한다. 다만 선언되지 않은 *파일* 수정은 기록하고, 계약을 사후 갱신한다.
- **Split:** Worker의 수정이 동시 실행 중인 다른 Worker의 `write_files ∪ reads`에 닿으면 `WRITE_SCOPE_CONFLICT`로 처리하고, 해당 Worker들을 직렬화한다. Exp1b 기준 빈도는 1/38이다.
- 파일 하나를 두 Worker가 함께 수정하는 것은 Split에서 허용하지 않는다. Mergeable artifact는 예외로, Core가 합친다.

### 5.5 고정비 제거 (우선 구현 대상)

| 고정비 | 현재 (Worker가 부담) | 목표 (Core가 한 번만 부담) |
| --- | --- | --- |
| repo 파악 | Worker가 rg/sed로 탐색 | Context Compiler가 계약의 graph slice(anchor, write_hints, reads의 본문과 signature)를 주입 |
| 빌드 | Worker 세션마다 따로 | revision별 공유 target dir와 증분 빌드. Core가 빌드 상태를 소유 |
| 검증 | Worker가 임의로 test 실행 | Core가 병합 후 한 번 실행. 범위는 전이적 R로 영향받는 테스트만 선택 |
| 재시도·escalation | 새 세션이 처음부터 | 실패 evidence와 slice를 다음 tier에 전달 |

Exp2b의 graph slice는 바뀐 파일 전체의 약 58%였다. 단 공용 symbol이 많이 끌려오는 p90에서는 1.56배였으므로, slice 깊이를 제한하고 원거리 symbol은 signature만 넣는다.

### 5.6 LOCAL 이관 (비용 절감의 주 경로)

원격 모델이 꼭 해야 하는 일은 의미 해석(anchor 확정, 애매한 요구)과 편집 판단뿐이다. 나머지는 LOCAL이 결정론적으로 하거나 로컬 모델이 맡는다.

| 작업 | 담당 | 원격 모델이 받는 것 |
| --- | --- | --- |
| 위치 후보 찾기 | LOCAL retrieval (ubis) | 상위 후보 목록 |
| 코드 읽기 | LOCAL graph slice | 필요한 symbol 본문과 signature |
| 빌드·테스트 | LOCAL runner | `TestResult` 구조화 요약 (실패 수, 주요 오류, 관련 줄, 원문 ref) |
| 기계적 수정 (rename, import, CHANGELOG, 포맷) | LOCAL 규칙 또는 로컬 소형 모델 | 없음 (Core가 직접 적용) |
| 편집 판단 | 원격 모델 (tier는 §5.7) | — |

품질 보호 장치:
- LOCAL이 만든 slice로 원격 모델이 부족함을 느끼면, 원격 모델은 추가 읽기를 요청할 수 있다. 이 요청 횟수를 기록해 slice 품질을 측정한다.
- 로컬 모델이 처리한 수정도 Core 검증을 똑같이 통과해야 한다. 실패하면 원격 tier로 승격한다.

### 5.7 Tier routing

- 분할의 비용 이득은 tier 차이에서만 나온다. 같은 모델을 쓰면 이득이 없었다 (Exp6).
- escalation은 `SMALL → MEDIUM → FLAGSHIP` 단조 승격으로 한다. 승격은 이산 상태(`UNRESOLVED / VALIDATION_FAILED / WRITE_SCOPE_CONFLICT`)에서만 한다.
- §5.5·§5.6이 먼저다. 고정비를 이관하지 않으면, 싼 모델로 바꿔도 탐색과 검증 출력은 그대로 남는다. 그러면 tier 이득도 고정비에 잠식된다.

### 5.8 데이터 계약 변경

```text
Task         += effect_contract, execution_mode, split_decision
WorkerTask   += context_slice_ref, write_files, owned_artifacts
WorkerResult += touched_files(실측), scope_events[]
CostRecord   += phase: Orient | Edit | Validate   // 고정비를 분리 측정
```

`phase` 구분이 핵심 telemetry다. "고정비 > 편집 비용"이라는 이번 결론을 운영 중에도 계속 확인할 수 있어야 한다.

## 6. 구현 기획서와의 차이

| 기획서 | 수정안 |
| --- | --- |
| §2-6 "독립 Task만 병렬 실행" | 독립이고 *이득이 측정될 때만* 병렬 실행. 기본은 Single |
| MVP 5 parallel scheduler | 후순위로 이동. Split Gate와 함께 구현 |
| MVP 8 Shared Memory / Context Compiler | 앞당김. 고정비 제거(§5.5)의 핵심 |
| §11 routing 신호 | `observed_fixed_cost`와 `est_work` 추가 |
| §13 benchmark 지표 | Orient / Edit / Validate 단계별 비용 추가 |

권장 우선순위: 단계별 비용 telemetry → Context Compiler(graph slice 주입) → Core 검증 일원화와 출력 구조화 → Effect Contract와 결정론적 확장 → LOCAL 기계적 수정과 tier escalation → Split Gate와 parallel scheduler.

## 7. 남은 검증 (하나만)

질문: **Core가 맥락을 주입하고 검증을 일원화하면 Worker 고정비가 줄어드는가?**

- 비교: Exp1과 같은 38 task에서, vanilla 단일 agent 대 graph slice를 주입받고 검증은 Core가 하는 단일 Worker.
- 판정 기준(사전 고정): 원격 input token이 50% 이상 줄고, 성공 하락이 2건 이내. Orient와 Validate 출력이 맥락의 98%이므로, 이관이 제대로 되면 50%는 보수적인 기준이다.
- 실패하면: LOOM의 효율 주장은 "Core 기반 cache/memory" 쪽으로 축소하고, 멀티 Worker 구조는 범위 관리 기능으로만 유지한다.
- 성공하면: 줄어든 고정비로 Split Gate 조건을 다시 계산한다. 같은 데이터로 Exp6를 재실행해 분할이 이득으로 바뀌는지 본다.

## 8. 한계

- R은 이름 기반 과대 근사다. 대상은 Rust 3개 repo이고, PR 규모 작업만 다뤘다 (대형 기능은 없음).
- 모델 실행은 task당 1회다. 비결정성은 반영하지 않았고, 성공률 1~2건 차이는 유의하지 않다.
- hidden test 판정이 가능한 task는 15건뿐이다 (inline test만 있는 PR은 build로만 판정).
- Exp3는 텍스트 수준 가환성만 검증했다.

재현 방법과 원자료는 `experiments/effect-contract/README.md`에 있다.
