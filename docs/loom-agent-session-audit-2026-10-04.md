# Native 에이전트 실행 감사 · 2026-10-04

사용자가 지적한 중심 설계는 AST/코드 그래프로 작업 의존성을 파악하고 Core가
독립 작업을 배분·회수·병합·검증하는 것이다. 당시 구현은 graph 후보를 제공했지만
실행 결정과 Native 표시에 Single을 고정했고, 실제 제어는 모델의 spawn/wait/interrupt
호출에 맡겼다. 도구가 있다는 것과 자동 오케스트레이션이 작동한다는 것은 달랐다.

## 실제 세션과 요약의 차이

검사한 세션은 `.loom/sessions/session-18db0f3c22c97aa0-11421.json`이다.
임시 파일 생성·찾기·삭제 작업의 trace는
`.loom/artifacts/693ddeeb767f36b6412f16ba0f74c606c0a6b7210331211f228e22b1282c0b90`이다.
기존 [SESSION_SUMMARY.md](../SESSION_SUMMARY.md)는 모델이 작성한 당시 출력으로 보존했다.

- 부모는 36/40회, 부모로 기록된 토큰은 140,179/1,000,000이다. 자식 비용이 당시
  부모 합계에 포함되지 않았으므로 이것을 전체 사용량으로 보아서는 안 된다.
  부모 토큰이 한도에 근접해 자동 중단했다는 요약은 근거가 없다.
- 마지막 상태는 사용자의 취소다. 여러 자식은 부모의 interrupt_agent 호출로
  취소됐다. 모든 Cancelled를 시간 초과나 리소스 제한으로 설명할 수 없다.
- 부모는 gemini-3.1-flash-lite인데 자식은 gemini-3.8-flash를 선택했다.
  자식 하나는 공급자의 하루 요청 할당량 429를 받았다.
- 부모는 1–5초의 짧은 wait를 반복하고, 작업 중인 자식에 send를 시도한 뒤
  interrupt/spawn을 반복했다. 완료된 핸들을 spawn 전에 회수하지 않아 종료된
  자식까지 네 개의 실행 슬롯을 차지할 수 있었다.
- merge_agent를 한 번도 호출하지 않았다. 격리된 자식 파일은 부모에서 보이지
  않으므로 부모의 반복 ls/find는 자식 결과를 확인할 수 없었다.
- `unknown Native activity`는 자식 ID를 부모의 영속 Task 목록에서 찾는 조회 오류다.
  에이전트가 실행되지 않았다는 뜻은 아니다.

## 연결한 실행 경로

1. 요청에서 얻은 심볼 anchor와 write hint를 그래프로 묶는다. 같은 쓰기 파일이나
   다른 그룹의 쓰기 파일을 읽는 의존성이 있으면 한 그룹으로 합친다.
2. 2–4개의 독립 그룹이 있고 그룹당 최소 세 호출을 배분할 수 있을 때 Core가
   직접 dispatch한다. 각 자식은 같은 revision에서 별도 사본과 파일 쓰기 lease를
   받는다. 한 번 만든 graph의 작은 signature slice를 공급한다.
3. 모델에게 맡기던 polling/merge는 Core에서 처리한다. 추가 모델 호출을 쓰지 않는다.
   한 자식이 미완료면 부분 성공을 원본에 적용하지 않는다. 전체 patch를 병합한 후
   기존 Core 검증·승인·충돌 보존 경로를 탄다.
4. 명시적으로 선택한 모델과 reasoning effort를 자식에게 전달한다. 수동 자식에도
   부모의 실제 선택을 전달한다. 전체 호출 한도·시간 한도와 관측 토큰을 공유하고,
   자식 요청·토큰·대기·phase 비용을 부모 결과에 합산한다.
5. 담당 범위를 system 지시와 자식 goal 앞부분에 명시한다. 자동 자식은 형제의
   변경이 없는 사본에서 전체 과제 검증을 수행하지 않는다. 실제 도구 쓰기 경계가
   지시를 무시하는 모델의 수정도 차단한다.
6. 자식의 running/proposed/merged/failed/cancelled 상태를 기록한다. Native는 실제
   Split과 그룹을 표시하고 자식 도구 기록을 조회한다. 재시작 뒤에도 trace에서 복원한다.

## 실모델에서 추가로 발견한 문제

Groq gpt-oss-120b로 clamp_score와 slugify를 고치는 작은 두 파일 과제를 실행했다.
첫 실행은 12호출·16,665토큰으로 실패했다. 한 자식은 TPM 429, 다른 자식은
형제 파일을 탐색·수정하려다 lease에 막혔다. 원본은 그대로였다.
[기록](../loom-core/measurements/graph-dispatch-live-2026-10-04.json).

담당 범위를 강화한 다음 실행은 7호출·5,975토큰으로 두 파일을 각각 고쳤지만,
잘못 생성한 완료 도구 이름의 오류로 종료됐다. 오류 JSON을 500자로 잘라 code를
잃었고 Router가 모델의 도구 지원을 잘못 비활성화했다. 응답의 code를 원문에서
먼저 보존하도록 수정했다. 별도 사본에서 두 자식 diff를 복원하면 숨은 9개 사례가
통과하지만, 이 결과는 Native의 완료·검증 성공이 아니다.
[기록](../loom-core/measurements/graph-dispatch-scoped-live-2026-10-04.json).

오류 코드 보존 후 실행에서도 두 파일은 수정됐지만 모델은 완료 JSON을 일반
EndTurn 응답으로 냈다. Worker가 같은 제안을 도구 호출로 재전송하도록 요구한 뒤
미완료로 종료했다. 이제 수정 이후 정확한 `summary` 문자열, `unresolved` boolean,
선택적인 `answer` boolean만 있는 EndTurn JSON은 제안 종료로 받는다. `answer=true`
라도 수정 이후에는 답변으로 취급하지 않는다. 기존 일반 문장 거부와 실제 patch의
독립 검증·승인 경계는 유지하며, 올바른 patch의 승인 대기와 잘못된 patch의 검증
실패를 함께 테스트했다. [기록](../loom-core/measurements/graph-dispatch-endturn-live-2026-10-04.json).

이어진 실행에서 한 자식은 세 호출로 제안을 끝냈지만 다른 자식은 미병합 사본에서
전체 check를 시도해 검증 명령 부재·반복 검사에 막혔다. 자동 자식에는 이제
`defer_validation_to_core`라는 명시적 실행 계약을 전달하고 check schema를 제공하지
않는다. 잘못 호출해도 실행하지 않고 `deferred_to_core`, `passed=null`을 반환한다.
화면에도 “병합 후 Core에서 수행 · 아직 미검증”으로 표시한다. 수동 자식이나
단일 Worker의 검증 능력은 유지한다.
[기록](../loom-core/measurements/graph-dispatch-validation-owner-live-2026-10-04.json).

### 실제 모델의 병합·검증·적용 확인

자식에게 전체 검증과 추가 위임 도구를 노출하지 않고 수정 제안에 집중하게 한 뒤,
Gemini Pro로 기존 함수 두 개를 수정하는 과제를 실행했다. 그래프가 두 파일을
별개의 작업으로 분할했고, 총 6회 모델 호출(입력 10,014·출력 999토큰)로 두
제안을 회수했다. Core가 병합한 결과는 숨은 9개 사례를 통과했다. 승인 전 원본이
유지된 것을 확인하고 승인한 후에도 같은 검사를 다시 실행해 통과했다.
[실행·승인·재검증 기록](../loom-core/measurements/graph-dispatch-pro-live-2026-10-04.json).

동일한 작은 과제에서 Groq 모델은 이후에도 완료 도구 이름 생성에 실패했다.
별도로 시도한 Sonnet 모델은 실제 공급자 카탈로그가 요청한 이름을 거부했다.
이는 공급자별 실행 결과이며, 성공한 한 과제를 일반적인 속도·완수율 개선이나
Core 변경만의 효과로 해석할 수 없다.
[Groq 기록](../loom-core/measurements/graph-dispatch-owner-live-2026-10-04.json),
[Sonnet 기록](../loom-core/measurements/graph-dispatch-sonnet-live-2026-10-04.json).

Native는 실제 Split 결정과 자식별 도구 이력을 표시하고, 완료 뒤 재시작해도
부모 trace에서 이를 복원한다. 실패한 모델 요청도 UI 호출 집계에 포함한다.
Shift+Enter 입력과 도구 이름의 한국어 진행 표시를 PTY/화면 검사에 포함했다.
실행 중이던 이전 Loom 창은 새 바이너리를 반영하지 않으므로 다시 시작해야 한다.

## 남은 한계

이 변경은 기존 코드에서 anchor/명시 파일을 찾을 수 있는 과제의 제한된 자동 분배다.
신규 파일 생성이나 “곳곳에 파일을 숨겨라”처럼 기존 그래프에 대상이 없는 요청을
의미적으로 분해하는 기획기는 아직 없다. 연결된 그룹의 순차 DAG 스케줄링과
네 개를 넘는 그룹의 wave 실행도 구현하지 않았다. 코드 그래프는 이름 기반 근사이며
독립성 증명이 아니다. lease·patch 충돌 검사·독립 검증이 마지막 경계다.

토큰 한도는 완료 응답의 관측 사용량으로 다음 요청을 제한한다. 동시에 이미 나간
요청의 토큰까지 사전 예약하는 엄격한 상한은 아니다. 분당 공급자 할당량의 병렬
backpressure와 실사용 과제의 paired baseline도 남아 있다. 이 소수 실행으로
일반 완수율, 속도, 토큰 효율이나 Codex 동급 품질을 주장하지 않는다.
