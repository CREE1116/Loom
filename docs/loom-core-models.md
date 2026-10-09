# Loom Core: 모델 계층

상태: 2026-10-02. Core 바텀업 구현의 첫 두 계층(L0 provider, L1 로컬 runtime)과 라우터(L2)다. 코드는 `custom-tui/src/core/model/`.

목표는 **같은 품질을 가장 싸게**다. 라우터는 항상 가장 싼 후보부터 시도한다.

## 계층

| 계층 | 파일 | 역할 |
| --- | --- | --- |
| L0 provider | `mod.rs`, `chat.rs`, `responses.rs`, `anthropic.rs` | 요청·응답·tool 호출·사용량을 하나의 형태로. 프로토콜 3개가 모든 provider를 덮는다 |
| L1 로컬 | `local.rs` | Hugging Face GGUF 다운로드(이어받기, sha256 검증, 메모리에 맞는 양자화 선택), Ollama pull, `llama-server` 실행·health check·종료 |
| L2 라우터 | `router.rs`, `discover.rs` | 자동 탐색, 싼 순서 라우팅, 실패별 cooldown, 호출 기록과 모델별 통계 |

| 프로토콜 | 덮는 provider |
| --- | --- |
| Chat Completions | Ollama, llama.cpp, LM Studio, OpenRouter, Gemini(OpenAI 호환 엔드포인트), Groq |
| Responses | OpenAI, opencodex 게이트웨이 |
| Anthropic Messages | Anthropic (API 키) |

사용량은 provider 간에 비교할 수 있게 맞춘다: `input`은 cache를 포함한 전체 입력, `cached_input`은 그중 cache hit. 보고되지 않은 값은 0이 아니라 없음으로 둔다.

## 라우팅 정책

1. 후보 정렬: 비용 등급(로컬 → 무료 → 구독 게이트웨이 → 가격 아는 유료 → 가격 모르는 유료), 같은 등급에서는 요청 tier를 만족하는 가장 작은 tier, 그다음 가격.
2. 요청 조건: 최소 tier, tool 필요 여부, 최대 비용 등급(예: `free`면 절대 유료로 넘어가지 않음).
3. 실패 처리:

| 오류 | 처리 |
| --- | --- |
| 인증 실패 | endpoint 전체 비활성화 |
| 할당량 소진 | `retry-after` 또는 1시간 cooldown |
| 호출 제한 | `retry-after` 또는 지수 증가 cooldown (30초부터) |
| 일시 오류 | 짧은 cooldown 후 다음 후보 |
| tool 미지원 | 그 모델만 tool 불가로 표시. tool 없는 요청에는 계속 사용 |

4. 모든 시도를 `~/.loom/model-usage.jsonl`에 기록한다. 모델별 호출 수, 실패, token, 비용, 평균 지연을 집계한다.

## 구독과 게이트웨이

- ChatGPT 구독: Codex runtime 경유 (현재 Loom 구조).
- Claude 구독: 공식 클라이언트(`claude -p`)를 worker로 붙인다. 구독 OAuth 토큰을 Loom이 직접 쓰지 않는다. Anthropic 약관상 서드파티 사용이 금지되어 있다.
- opencodex: 사용자가 설치·설정한 게이트웨이를 Responses endpoint로 호출한다. 구독 계정과 그 위험은 opencodex 설정에서 사용자가 관리한다.
- codex-chatgpt-web bridge: Codex 내부 메타데이터를 요구해 직접 호출하지 않고 Codex runtime으로만 쓴다.

## 명령

```sh
custom-tui --models [--tier medium] [--max-cost free]   # 탐색 결과와 라우팅 순서
custom-tui --ask "질문" [--tier ...] [--max-cost ...]     # 가장 싼 모델로 질문
custom-tui --model-stats                                 # 모델별 사용량 통계
custom-tui --pull hf:Qwen/Qwen2.5-Coder-7B-Instruct-GGUF # 메모리에 맞는 양자화 자동 선택
custom-tui --pull hf:owner/repo/file.gguf                # 파일 지정
custom-tui --pull ollama:qwen2.5-coder:7b
custom-tui --serve hf:owner/repo --port 8080             # 다운로드 후 llama-server로 서빙
```

API 키는 환경 변수에서만 읽는다: `OPENROUTER_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `HF_TOKEN`(비공개 모델).

## 검증

- 프로토콜별 요청 형식·응답 파싱·tool 왕복·사용량 정규화: 로컬 가짜 서버 테스트.
- 실제 호출: 로컬 Ollama(Gemma 4 E4B)로 tool 호출 → 결과 → 최종 답변, 두 번째 호출 cache hit 확인. Hugging Face에서 Qwen2.5 1.5B를 받아 `llama-server`로 서빙하고 라우터가 자동 탐색해 호출.
- 라우터: 싼 순서, tier 필터, 비용 상한, 호출 제한 후 fallback과 cooldown, 인증 실패 비활성화, tool 미지원 표시.

## 다음

L3 worker loop: Core가 소유하는 에이전트 루프. 도구는 [Core 코드 도구 설계](loom-core-tools.md)의 `find`/`outline`/`read`/`edit`/`check`. 라우터를 통해 모델을 고르고, telemetry로 단계별 비용을 기록한다.
