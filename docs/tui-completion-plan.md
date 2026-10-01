# Loom TUI 완성 계획

이 문서는 [전체 구현 기획서](orchestrator-implementation-plan.md)의 UI 단계별 구현 목록이다. TUI를 먼저 완성하되 UI가 모델 세션·Codex protocol·작업 통제를 소유하지 않는다. UI는 `AgentEvent`를 표시하고 `AgentCommand`로 사용자의 의도를 전달한다. Core 교체 중에도 화면과 조작을 유지한다.

## 필요한 기능과 현재 상태

| 기능 | 상태 | 완료 기준 |
| --- | --- | --- |
| 대화·stream·고정 입력창 | 구현 및 기존 회귀 검증 | 긴 응답 중에도 입력·스크롤이 반응하고 초안 유지 |
| 여러 줄·Unicode·붙여넣기·복사 | 구현 및 기존 회귀 검증 | 한글·결합문자 편집, 원문 그대로 전송, 붙여넣기 표시 축약 |
| 키보드·마우스·좁은 화면·resize | 구현 및 기존 회귀 검증 | 핵심 기능을 키보드만으로 수행, 클릭과 키보드가 같은 동작 |
| 작업 중 애니메이션·상태 문구 32개 | 구현 | 출력이 없어도 움직이고 완료·승인·blocking 질문 대기에는 정지 |
| 선택지·직접 입력 질문 | 구현 | 여러 질문, 설명·이전/다음, 답변 검증·제출·실패 재시도, 초안 보존 |
| 민감한 입력 | 구현 | UI에는 마스킹, 자동 선택·자동 제출·대화 기록 삽입 없음 |
| 승인·신뢰·권한 | 구현 및 기존 회귀 검증 | 질문과 별도 경계, adapter가 승인 payload 소유, 중복 응답 방지 |
| 세션 목록·전환·분기·자동 재개 | 과도기 Codex backend 구현 | 실행·질문·승인 대기 중 안전한 전환 방지, 초안 유지 |
| 대기열·중단·우선 실행 | 구현 및 기존 회귀 검증 | 실패 시 지시 보존, 전송 중복 방지, 취소 결과를 확인한 뒤 다음 전송 |
| 도구 요약·원문 페이지·파일 변경 | 구현 및 기존 회귀 검증 | 상세를 닫아도 읽던 위치 유지, raw output은 필요할 때만 보기 |
| 공유 LOCAL 탐색 | 구현 | UI/CLI, 네 동시 소비자의 동일 탐색 1회, revision별 evidence와 제한 표시 |
| 활동: worker·상태·대기 이유 | UI projection·실제 LOCAL·Mock 구현 | running/completed/blocked/failed/cancelled 구분, 기다리는 dependency와 이유 명시 |
| TaskGraph·critical path·상세 | Mock TaskGraph UI 구현; 전체 Core scheduler 후속 | Core가 지정한 critical 표시, inputs/outputs/elapsed 상세, 미계측 표시; 패널을 닫아도 정상 사용 |
| 토큰·cached input·할당량·effort | runtime 보고값 연결; 가격·cost·cache epoch 후속 | 실제 usage와 가격이 있을 때 표시, 미계측을 0 또는 절약 추정으로 꾸미지 않음 |
| 연결 오류·종료·미전송 보존 | 초기 복원 충돌·종료 정리 구현; reconnect 후속 | 오류 원인·복구 동작 제공, 자동 재전송 없음, 구독 복구 후 상태 확인 |
| 자체 Core 세션·중단 후 상태 복구 | durable state 구현 후 연결 | 모델 history 없이 goal/task/decision/evidence에서 재개 |
| Markdown·코드·대화 구분선 | 구현; 파일 탐색 후속 | 경로·코드 블록 탐색, 본문과 전송 원문 보존 |
| 접근성·언어·설정 일관성 | 단계별 점검 | 색 없이 상태 구분, 짧은 화면에서도 제출·취소·복귀 접근 |

## 순차 구현

1. **입력 상호작용 완성**: typed question/answer 계약, blocking/nonblocking 질문, 명시적 선택·직접 입력, 질문별 draft, secret masking, adapter 검증, authoritative resolution. 로컬 WebSocket fixture와 PTY로 왕복 확인한다.
2. **진행·대기 표현 완성 (초기 UI 구현)**: 대화에는 현재 상태만 간결하게, Activity에는 worker·dependencies·실패/취소 원인을 표시한다. mock 이벤트로 레이아웃을 완성한 뒤 실제 TaskGraph를 연결한다.
3. **복구 동작 완성**: 끊김·응답 실패·중단 실패에 복구 경로를 제공한다. reconnect가 turn 제출·승인·질문 답변을 자동 재전송하지 않도록 한다.
4. **세션·검토 경험 완성**: 자체 상태 저장 경계, 변경 검토·코드/파일 참조, 긴 기록 탐색을 연결한다. 단순 Codex conversation resume를 자체 context continuation으로 표기하지 않는다.
5. **운영 정보 연결**: 비용·cache·session epoch·context sources를 실제 Core 계측으로 연결한다. 기본 화면에는 필요한 요약만 제공한다.

## 첫 단계의 검증 조건

- 기본 선택지는 사용자의 확인 전에는 답변이 아니다. 미답변·중복 ID·잘못된 선택지·다른 session의 응답은 거부한다.
- 키보드와 클릭 모두 같은 질문 state machine을 사용한다. 질문 입력이 대화 초안을 덮어쓰지 않는다.
- Esc/나중에는 질문을 숨길 뿐 답변하거나 취소하지 않는다. `/questions` 또는 질문 알림으로 다시 연다.
- 제출은 한 번만 전송하며 서버의 resolved 이벤트까지 대기한다. 실패하면 입력을 유지한 채 재시도한다.
- blocking 질문은 실행 애니메이션을 멈춘다. nonblocking 질문은 실행을 중단시키지 않는다. 타이머나 기본값으로 자동 제출하지 않는다.
- 질문·승인·신뢰 창의 우선순위와 session scope를 유지한다. 미완료 질문이 있으면 세션 전환·분기를 막는다.
- 데모의 `질문 데모` 입력으로 모델 호출 없이 전체 흐름을 확인한다.

## 구현 범위의 표시

첫 배포는 자체 TUI와 event boundary, LOCAL 탐색, Codex adapter를 제공한다. 네 remote worker 실행, durable CanonicalState, patch isolation/merge, 장기 RAG, OpenRouter routing은 전체 기획서의 다음 단계다. 이 기능들이 완성되기 전에는 화면에 가짜 실행·가짜 비용 절약 수치를 표시하지 않는다.

## 두 번째 단계의 현재 구현

`ActivityTask`는 UI용 읽기 projection이며 TaskGraph의 Source of Truth가 아니다. Core가 worker/status/dependencies/reason/critical/elapsed/inputs/outputs를 이벤트로 제공한다. UI는 critical path나 실행 권한을 판단하지 않는다.

실제 LOCAL 탐색은 running/completed/failed 이벤트, 측정된 elapsed, 결과 evidence reference를 연결한다. `/task N` 또는 작업 행으로 상세를 열고 초안을 유지한다. Mock Core의 `활동 데모`는 LOCAL/MEDIUM/SMALL labels와 blocked dependency, critical 표시, 중단 후 cancelled 전환을 제공한다. 실제 remote worker scheduling은 아직 없으며 미계측 비용·cache를 0이나 절약률로 표시하지 않는다.

## 진행 중인 세 번째 단계

자동 복원 active-writer 충돌은 기존 기록을 삭제하지 않고 별도 세션으로 복구한다. 최초 연결 실패에서 `/new`를 선택해도 미전송 지시를 유지한다. 새 세션 전환은 기존 대화의 대기열을 가져오지 않는다. WebSocket 업그레이드는 3초 startup timeout을 쓰고 연결 후에만 10ms poll timeout으로 전환한다.

메인 종료는 새 작업을 막고 진행 중인 turn/start 결과를 확인한 뒤 interruption과 unsubscribe를 요청한다. 자신이 시작한 runtime은 Unix process group / Windows process tree로 정리한다. 외부 서버와 다른 Loom의 프로세스는 종료하지 않는다. 통신 Client가 drop되면 reader thread와 socket도 닫힌다. 저장된 기록은 유지한다. 프로세스 강제 종료·장기 durable task/queue 복구는 아직 별도 단계다.

Windows CMD/PowerShell 시작 파일, npm shim→Node 진입점, executable 탐색, USERPROFILE trust 경로, 클립보드·새 콘솔 경로를 추가한다. Windows CI에서는 Rust 검증과 실제 Codex npm runtime의 모델 호출 없는 probe를 실행한다. Unix PTY 회귀 검증을 Windows 대화형 콘솔 검증으로 대신 주장하지 않는다.
