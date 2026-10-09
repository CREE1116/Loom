# 공개 에이전트 구현 비교와 V3 수정

비교 대상의 실제 코드와 공개 문서를 읽고 V3에서 재현되는 결함을 고쳤다.
외부 구현을 그대로 복사하거나 기존 Codex 소스를 변경하지 않았다.

| 참고 구현 | 확인한 설계 | Loom 반영 |
| --- | --- | --- |
| [Codex 도구 실행](~/code/Custom_cli/codex-rs/core/src/tools/parallel.rs), [문맥 관리](~/code/Custom_cli/codex-rs/core/src/context_manager/history.rs) | 복구 가능한 도구 오류를 모델 응답으로 돌려주고, 모델 문맥과 보존 상태를 구분 | 최신 호출·결과 묶음을 지키고 실패 피드백을 다음 요청에 전달 |
| [OpenCode processor](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/session/processor.ts), [compaction](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/session/compaction.ts) | 도구의 실행·성공·오류 상태를 보존하고 최근 도구 결과를 먼저 보호 | 큰 결과 때문에 최신 교환 전체가 사라지는 결함 수정. 반복 호출도 모델에 피드백 후 다른 행동으로 복구할 기회 제공 |
| [Grok Build 출력 제한](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/util/truncate.rs), [완료 규율](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-agent/src/system_reminder.rs) | UTF-8 안전한 출력 미리보기·전체 크기·복구 힌트, 제한된 완료 재확인 정책 | 출력 미리보기와 원문 artifact 조회 분리. 수정 없는 완료를 같은 사본에서 두 번까지 교정 |
| [Antigravity subagents](https://www.antigravity.google/docs/subagents/) | 격리 문맥, 워크스페이스 선택, 부모의 권한 상속과 비동기 수명주기 | 격리 워커에 빠져 있던 프로젝트 지침 전달. Root 및 쓰기 범위의 AGENTS.md를 제공하고 새 하위 폴더의 지침을 수정 전에 전달 |

Antigravity는 공개 문서를 비교한 것이며 내부 엔진을 읽었다고 주장하지 않는다.
Grok의 TodoGate는 기본적으로 비활성이다. 여기서는 그 정책 분리·재확인 개념을
Loom의 실제 패치 유무 판정에 맞게 별도로 구현했다.

## 수정한 동작

- 12 KB보다 큰 최신 교환을 통째로 버리던 문맥 구성을 교체했다. 호출 ID와 결과의 짝을 보존하고 출력 텍스트부터 줄인다. 구조화된 호출 인수가 매우 크면 원본 인수를 우선 보존하므로 12 KB는 전체 요청의 엄격한 토큰 상한이 아니다.
- 큰 출력은 최대 4 KB 미리보기와 원문 id를 전달한다. `read_artifact`가 원문의 바이트 구간을 조회한다. UI·Core 기록과 모델 문맥을 구분하며 원문은 `.loom-v3/artifacts`에 보존한다.
- artifact는 내용 주소로 중복을 줄이고 병렬 저장을 원자적으로 처리한다. 조회는 id·구간·일반 파일·내용 해시를 검사한다. 원본 소스 revision과 쓰기 범위는 바뀌지 않는다.
- 파일 읽기 후 별도로 파일 전체의 첫 부분을 모델에 주입하던 코드를 제거했다. 실제 읽은 구간만 관찰로 남긴다. 파일 변경 시 이전 결과 캐시를 무효화하고 최신 교환과 겹치는 캐시를 중복 전달하지 않는다.
- 같은 인수·파일 상태의 호출은 재실행 대신 피드백을 주고, 계속 반복하면 이유와 함께 중단한다. 잘못된 완료는 새 사본을 만들지 않고 기존 사본에서 교정한다. 요청 문구를 정규식으로 분류하지 않고 도구 효과·실제 패치·명시된 작업 계약을 사용한다.
- 지침은 Root에서 하위 폴더 순서로 제공한다. 알려진 쓰기 범위는 첫 요청에 포함한다. 새 범위에서 지침을 발견하면 수정 전에 모델에 전달하며, 이 재시도를 무의미한 반복으로 세지 않는다. 지침이 Core의 읽기 전용 권한을 바꾸지 못하는 것도 검사했다. 읽기 전용 하위 작업의 모든 탐색 범위에 대한 지침 수집까지 구현한 것은 아니다.

## 실제 실행

| 과제 | 변경 전 | 변경 후 |
| --- | --- | --- |
| 긴 README의 마지막 식별값, 로컬 Gemma 4B | 정답, 3회 호출, 입력 2,715 / 출력 1,327, 71.197초 | 정답, 2회 호출, 입력 2,358 / 출력 349, 22.596초 |
| clamp 경계·뒤집힌 범위 수정, 로컬 Gemma 4B | 첫 호출 시간 초과 | 두 번째 호출 시간 초과, 미완료 |
| 같은 수정 과제, OmniRoute auto | 비교용 이전 cloud 실행 없음 | 3회 호출, 수정 제안·승인·숨은 테스트 통과, 실행 34.497초 |

Cloud 경로의 실제 모델은 `antigravity/gemini-3.8-flash-tiered`와
`antigravity/gemini-3.1-pro-low`였다. 단일 모델 성능 비교로 취급하지 않는다.
로컬 문서 과제는 한 번씩 실행한 파일 기반 비교다. 모델 워밍·실행 순서가 지연에
영향을 주므로 일반적인 속도 개선율을 주장하지 않는다. 로컬 수정 실패도 보고서에 남겼다.
첫 로컬 수정 실행은 release 빌드와 겹쳤고, 빌드 종료 후 재실행도 실패했다.

라이브 실행은 명시적인 Inspect/Edit 계약을 사용했다. Auto의 계획·검토·병렬 경로는
기존 회귀 검사와 실제 PTY의 명시적 모델 fixture로 확인했다. 모든 모델과 자연어 요청이
완료된다는 근거나 Codex 수준의 전반적인 성능 입증으로 해석하면 안 된다.

Core 회귀 검사는 큰 결과 뒤의 실패 복구, 호출·결과 짝, UTF-8, 원문 조회,
반복 복구, 완료 교정, 병렬 artifact 저장과 손상 감지, 워커 지침 전달을 다룬다.
화면 검사는 원문 조회의 이름·구간, 기존 병렬 대화·승인·취소·세션 복원을 확인한다.
규칙 전달 fixture에는 빌드 manifest가 없어 실제 변경값을 확인하는 grep 검증을 추가했다.

수치와 실패 기록: [벤치 보고서](../bench/results/loom-reference-hardening-2026-10-05.json).

최종 소스에서 Core 69개·TUI 233개 테스트가 통과했고 실패는 없다. 수동 측정·창 실행용
검사 4개는 제외됐다. 화면 자체 검사 483개와 release 바이너리의 실제 PTY 검사도
통과했다. PTY는 큰 출력의 원문 조회 및 V2/V3 격리 수정·승인 복원·원본 반영을 확인한다.
release 빌드가 완료됐으며 기존 Loom 창은 재시작해야 새 구현을 사용한다.
