# Loom Core 코드 도구 설계 (오픈소스 조사 기반)

## 구현 현황 (2026-10-07)

- `map`: Core 소유 snapshot/graph에서 경로별 파일, 언어, 핵심 심볼, **이름 기반 근사** 파일 의존성을 출력한다. `path`와 `budget`으로 범위를 제한한다.
- `outline`/`read`/`refs`: Rust(syn)를 유지하면서 Python, JS/TS/TSX, Go, Java(tree-sitter tags)의 심볼 ID를 공유한다. 파서별 차이를 `world::lang`으로 격리했다.
- `ast_search`: 내장 ast-grep Rust 라이브러리로 언어별 구조 패턴을 찾는다. `pattern`/`path`/`language`/`limit`을 지원하며 탐색 결과를 Loom 심볼 ID에 연결한다.
- `symbols`/`symbol_context`: 이름·종류·테스트 여부로 AST 심볼을 좁힌 뒤, 한 호출에서 대상 심볼의 owner·같은 파일의 인접 심볼·직접 caller/callee를 확인한다. V4는 이 구조적 문맥을 broad text search보다 먼저 사용한다.
- `read_many`: 이미 찾은 심볼 또는 좁은 파일 범위를 최대 8개까지 한 호출로 읽는다. 전체 출력 상한을 항목별로 공평하게 나눠 한 대상이 나머지 문맥을 밀어내지 않으며, 각 파일의 scoped instruction도 보존한다.
- `plan_split`: 지정된 심볼들의 동일 파일 쓰기 및 1-hop 그래프 의존성을 기준으로 연결된 작업 묶음을 제안한다. 결정적 연결요소만 계산하며 결과는 `advisory=true`로 명시한다.
- `spawn_agent`의 명시적 `write_files`/`read_files`: 형제 작업의 선언된 파일 범위 충돌 시 생성을 거부하고, 부모의 예약 파일 수정을 차단한다. 자식은 Core 편집 도구로만 선언된 파일에 쓰며, 실행 명령을 제한한다. 선택적 `anchors`를 주면 현재 revision에서 8KB 이하 그래프 slice를 자식에게 제공한다. 자식의 실제 patch 파일 목록을 병합 전에 다시 검증한다. 기존 범위 미지정 `spawn_agent` 호출은 호환된다.

모든 탐색 결과는 동일한 요청 중 같은 repository snapshot에 고정된다. tree-sitter tags에서 추출한 참조와 `map`의 의존성은 이름 기반 근사치다. **작업 분할의 의미적 독립성이나 충돌 없음에 대한 증거로 사용하면 안 된다.** 현재 file lease는 한 부모의 명시적으로 범위를 지정한 하위 작업에 적용된다. 다른 최상위 작업까지 예약을 공유하는 중앙 lease 테이블과 영속 TaskGraph 스케줄러는 미구현이다. `read_files`에 누락한 동적 의존성은 graph가 증명하지 못한다. 최종 patch 범위 및 3-way 병합 검증은 필수다. 측정 gate를 통과하기 전 자동 분할은 보류한다.

### V4 fast path

`loom-core-v4`는 위 도구 중 AST 탐색과 정밀 편집만 사용하는 **단일 워커 fast path**다. `plan_split`, child-agent, `update_plan`은 V4 도구 표면에서 제거해 V3의 coordination stack이 암묵적으로 되살아나지 않게 했다. 변경은 overlay에 격리되고 Core 검증 뒤 승인 대기 상태로 저장되며, 승인 시 `Patch::apply`가 검증 때의 base revision과 현재 workspace가 같은지 다시 확인한다. 따라서 승인 전에 사용자가 원본을 바꾸면 새 바이트를 덮어쓰지 않고 pending proposal을 유지한다.

상태: 2026-10-07. Core가 직접 소유할 읽기·검색·편집 도구의 설계 근거다. 조사 대상은 2026-10 기준 각 저장소의 main/dev 브랜치 소스다.

## 1. 왜 도구를 바꿔야 하나

Codex의 코드 도구는 두 개다: `apply_patch`와 셸 실행. 읽기·검색 전용 도구가 없어서 모델이 `rg`와 `sed -n '1,460p'`로 파일을 통째로 읽는다. 실제 Loom 세션 telemetry에서 원격 맥락의 87%가 이 탐색 출력이었고, 편집은 4%였다 ([실험 결과](loom-experiment-findings.md) §2.5).

Core가 도구를 소유하면 탐색 출력의 크기와 모양을 Core가 정한다. 비용 절감의 실제 지렛대가 여기다.

## 2. 조사 결과

| 프로젝트 | 읽기 | 검색 | 편집 | 편집 후 검증 |
| --- | --- | --- | --- | --- |
| **Codex** | 셸 (`sed`, `cat`) | 셸 (`rg`) | `apply_patch` (컨텍스트 앵커 기반 patch 형식) | 없음 (모델이 직접 테스트) |
| **Claude Code** | `Read`: offset/limit, 줄 번호, 편집 전 읽기 강제 | `Grep`(ripgrep, 출력 모드: 파일 목록/내용/개수, head_limit), `Glob` | `Edit`: 정확한 문자열 치환, 유일해야 함, `replace_all` | 없음 |
| **opencode** | `read`: 기본 2000줄, 줄당 2000자, 50KB 상한, "30줄씩 잘게 읽지 말 것" | `grep`(ripgrep), `glob`, `lsp` | `edit`: 정확 치환 + **퍼지 대체 9단계** (줄 trim, 블록 앵커, 공백 정규화, 들여쓰기 유연, escape 정규화, 경계 trim, 문맥 인식, 다중 발생), `apply_patch`, `write` | **편집 직후 LSP 진단**을 결과에 붙임 ("LSP errors detected, please fix") |
| **SWE-agent** | `open`/`goto`/`scroll`: 고정 크기 창(window) | `search_dir`: **100개 파일 초과 시 거부**하고 좁히라고 요구. `find_file`, `filemap`(긴 함수 본문 생략) | `edit start:end`: 줄 범위 치환 | **편집 전후 flake8 비교, 새 오류만 있으면 편집을 되돌리고** 적용본·원본 창을 함께 보여줌 |
| **OpenHands** | `view`: view_range, 응답 길이 상한, 디렉터리 2단계 | (셸) | `str_replace`(유일), `insert`(줄 뒤), `create`, **`undo_edit`**. 결과로 편집 주변 snippet 표시 | 선택적 lint |
| **Serena** | `get_symbols_overview`, `find_symbol`(이름 경로, depth, include_body) | `find_referencing_symbols`, `find_implementations`, `search_for_pattern` | **심볼 단위**: `replace_symbol_body`, `insert_after/before_symbol`, `rename_symbol`, `safe_delete_symbol`. 정규식 치환 (`begin.*?end`로 큰 구간을 인용 없이 치환) | 편집 도구마다 LSP 진단 |
| **Aider** | repo map (tree-sitter + 중요도 순위) | — | 형식 다수: whole / SEARCH-REPLACE / udiff. **적용 전략 다단계**: 정확 → 공백 무시 → `...` 생략 처리 → 편집 거리 최근접 → diff-match-patch | lint·테스트 후 자동 수정 루프 (선택) |
| **Cline** | `read_file` | `search_files`(regex), `list_code_definition_names` | `replace_in_file`: SEARCH/REPLACE 블록, 실패 시 파일 최신본을 다시 주고 "블록 5개 이하로" 재시도, 3회 실패 시 `write_to_file` | 에디터 진단 |

공통으로 확인된 사실:

1. **정확한 문자열 치환이 기본**이고, 모델 실수(공백·들여쓰기·escape)를 흡수하는 **퍼지 대체 단계**가 붙는다 (opencode, Aider, Cline).
2. **출력 상한과 "좁혀라" 응답**: 검색·읽기 결과가 크면 잘라내는 대신 범위를 좁히라고 돌려보낸다 (SWE-agent 100파일, opencode 50KB).
3. **편집 직후 검증**: 새 오류만 골라 즉시 알려주거나(opencode·Serena LSP), 아예 편집을 거부한다(SWE-agent).
4. **심볼 단위 도구**가 줄 단위보다 짧고 정확하다 (Serena). 본문 전체를 인용하지 않아도 된다.
5. **편집 전 읽기 강제**와 undo가 사고를 줄인다 (Claude Code, opencode, OpenHands).

## 3. Loom Core 도구 설계

Loom에는 이미 code graph(`core/graph.rs`: 심볼 id, span, 참조 해석)와 telemetry가 있다. 도구는 이것을 기반으로 한다.

| 도구 | 동작 | 근거 |
| --- | --- | --- |
| `find` | 텍스트·정규식·심볼명 검색. 결과를 **줄이 아니라 심볼 단위로 묶어** `id · signature · 줄` 형태로 반환. 상한을 넘으면 개수와 함께 "범위를 좁혀라"를 반환 | SWE-agent 상한, Serena 심볼 검색. Orient 출력 축소 |
| `outline` | 파일 또는 모듈의 심볼 목록과 signature, 본문은 생략 | SWE-agent `filemap`, Serena overview. **파일 통째 읽기 대체** |
| `read` | 심볼 id로 본문 읽기, 또는 파일+줄 범위. 줄 번호 포함. 큰 파일을 범위 없이 요청하면 outline을 돌려줌. 읽은 내용의 digest를 기록 | Claude Code·opencode read, 편집 전 읽기 강제 |
| `refs` | 이 심볼을 참조하는 쪽 / 이 심볼이 참조하는 쪽 | graph `referrers`·`resolve`, Serena |
| `edit` | `old → new` 정확 치환(유일, `replace_all`). 실패 시 퍼지 단계(줄 trim → 공백 정규화 → 들여쓰기 유연 → 블록 앵커)로 재시도하고, **어느 단계로 맞췄는지 결과에 표시**. 결과로 편집 주변 snippet 반환 | opencode·Aider·Cline |
| `replace_symbol` / `insert_after_symbol` | 심볼 id로 본문 전체 교체 또는 뒤에 삽입. span은 graph가 제공 | Serena |
| `apply_patch` | Codex 형식 다중 파일 patch. 기존 모델 습관과 호환 | Codex |
| `undo` | 마지막 편집 되돌리기 (파일별 스택) | OpenHands |
| `check` | Core가 실행하는 빌드·테스트. 구조화된 `TestResult`(실패 수, 주요 오류, 관련 줄, 원문 ref)만 반환 | 기획서 §9, 실험 Validate 9~37% |

모든 편집 도구에 공통으로 붙는 Core 동작:

1. **즉시 구문 검증**: 편집 결과를 파싱해서(Rust는 `syn`, 비용 거의 0) 새 구문 오류면 편집을 거부하고 오류 위치와 함께 돌려준다. SWE-agent처럼 **편집 전후를 비교해 새 오류만** 판단한다.
2. **범위 감시**: 편집이 닿는 심볼을 graph로 계산하고, 동시 실행 중인 다른 작업의 lease(W∪R)와 겹치면 `WRITE_SCOPE_CONFLICT`로 거부한다. Codex 위에서는 감지만 가능했던 것을 여기서 강제한다.
3. **편집 전 읽기 강제**: 마지막으로 읽은 digest와 현재 파일이 다르면 거부하고 다시 읽게 한다. 다른 세션이 파일을 바꾼 경우도 여기서 잡힌다.
4. **telemetry**: 도구 호출마다 Orient/Validate/Edit 단계와 출력 바이트를 기록한다.

## 4. 측정 계획

같은 실험 task(PR 38건)에서 Codex 도구 세트와 Loom 도구 세트를 비교한다: 성공률(hidden test), 원격 input token, Orient 바이트, 편집 실패·재시도 횟수, 퍼지 단계 사용 빈도. 퍼지 단계와 즉시 구문 검증은 개별로 끄고 켜서 효과를 따로 본다.

## 5. 모델 라우팅과 사용량 통계

라우터(OmniRoute식 자동 선택)는 호출마다 요청한 tier, 실제로 응답한 provider·모델, input/cached/output token, 가격, 지연, 실패 사유를 기록한다. 활동창에서 모델별로 호출 수, token, 비용, 실패율, 평균 지연을 집계해 보여준다. 가격을 모르면 비용은 "미확인"으로 표시하고 0으로 처리하지 않는다.
