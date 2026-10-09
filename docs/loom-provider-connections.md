# Loom 공급자 연결

Loom은 등록한 공급자에 직접 요청하고 자체 라우터로 모델을 고른다. OmniRoute가 없어도 동작하며, 기본 실행에서는 OmniRoute를 검색하거나 시작하지 않는다. `LOOM_OMNIROUTE=1`은 이전 연결을 위한 선택 사항이다.

## 대화 화면에서 등록하기

`/provider` 또는 **환경 → 연결**에서 관리한다.

- **연결 추가**: OpenAI, Anthropic, Gemini, Groq, OpenRouter, DeepSeek, OpenAI 호환 서버를 등록한다. 같은 공급자도 서로 다른 이름으로 여러 계정을 등록할 수 있다.
- API 키는 별도 입력 폼에서 가려진다. 대화, 입력 히스토리, 진단 파일에 보내지 않는다. `env:OPENAI_API_KEY`처럼 환경변수를 참조하거나 인증 없는 서버에 `none`을 사용할 수 있다.
- 서버 주소는 기본값을 쓰려면 `default`를 입력한다. 등록 후 사용 가능한 모델을 조회하고 모델 선택 화면으로 이동한다. 모델 이름을 미리 알 필요가 없다. 목록 조회를 지원하지 않는 서버는 CLI의 `--model`로 직접 지정할 수 있다.
- 모델 화면 맨 위의 테두리 있는 **검색** 입력칸에 이름·공급자를 입력한다. 커서와 검색 결과 수가 표시되고 긴 검색어는 입력 위치에 맞춰 좌우로 이동한다. 검색은 대화 초안과 분리되어 있다. 가격별/공급자별 묶음을 전환하고 **무료만**으로 목록을 좁힐 수 있다. 가격은 USD/100만 토큰 단위이며 출력 단가로 묶고 입력·출력 단가를 함께 표시한다. 가격 정보가 없으면 **가격 미확인**이다.
- OpenRouter는 계정에 허용된 모델 목록과 실제 단가·도구 지원을 조회한다. 무료 자동 라우팅을 선택하면 도구를 지원하는 무료 모델을 Loom이 고른다. **무료만**은 목록 필터이며, **자동 라우팅도 무료로 제한**은 실행 비용 상한을 바꾼다.
- **자동** 연결은 Loom이 비용과 가용성을 보고 선택한다. **직접 선택** 연결은 `/model 연결이름/모델ID`로 고를 때만 사용한다.
- **ChatGPT 계정 연결**은 Loom 자체의 브라우저 로그인이다. 계정이 허용하는 ChatGPT 요금제 사용 권한을 승인해야 사용할 수 있다. 다른 앱의 로그인 파일을 가져오지 않는다.
- 등록 폼을 숨겼다면 **등록 계속**, 취소하려면 **등록 취소**를 선택한다.
- 연결을 새로고침하면 다음 작업부터 반영된다. 진행 중인 작업과 병렬 질문은 기존 연결로 끝난 뒤 새 목록을 적용한다.
- `/model`을 열면 설치·실행 상태를 다시 조회한다. Ollama에 설치한 모델은 Ollama 서버가 실행 중이어야 조회된다. 연결이 꺼져 있으면 모델 화면에 원인과 **다시 조회**를 표시한다. Loom 내장 엔진용 모델은 별도로 `/runtime`에서 설치한다.

`/provider login chatgpt personal`로 여러 ChatGPT 연결을 만들 수 있다. 연결 화면에서 재로그인·로그아웃·사용량 관리가 가능하다. 브라우저 로그인과 원격 로그아웃은 입력을 막지 않는 별도 작업으로 수행한다.

## CLI에서 관리하기

**환경 → 자동 라우팅** 또는 `/routing`에서 비용 상한과 연결별 자동 선택을 관리한다. 모델을 고정해서 사용 중이라면 같은 화면의 **자동 선택**을 눌러 자체 라우팅으로 돌아간다. 무료 목록 필터와 실행 비용 상한은 별도 설정이다.

```sh
loom provider --help
loom provider templates
loom provider add openai --name work --key-env OPENAI_API_KEY
loom provider add compatible --name local-server --base-url http://127.0.0.1:8080/v1 --model my-model --cost local
loom provider login chatgpt --name personal
loom provider list
loom provider test work
loom provider disable work
loom provider enable work
loom provider logout personal
loom provider remove work
```

키를 저장하려면 `--key-stdin`으로 표준 입력에서 읽는다. 키 자체를 명령 인자로 받지 않으므로 쉘 히스토리에 붙여 넣을 필요가 없다. 같은 이름을 다시 등록하면 설정과 인증을 교체한다. 새 키나 환경변수를 지정하지 않으면 예전 키를 제거한다.

등록 설정은 `~/.loom/providers.toml`, API 키는 별도의 `~/.loom/provider-credentials.json`에 저장한다. 두 파일은 사용자만 읽을 수 있는 권한(0600)으로 원자적으로 교체한다. ChatGPT 등록과 토큰은 `~/.loom/chatgpt-oauth/`에 분리한다. `LOOM_MODEL_STATE_DIR`로 저장 위치를 지정할 수 있다.

ChatGPT 로그인에 쓰는 설치 식별자는 공식 UUIDv4 URN 형식(`urn:uuid:…`)으로 한 번 생성하고 재사용한다. 이전 Loom이 만든 잘못된 `loom-…` 형식은 다음 로그인 시 복구한다. 이미 인증된 다른 계정의 자격증명이나 클라이언트 ID는 덮어쓰지 않는다. `invalid_authorize_request`의 `ext_agent_host_id` 오류가 났다면 최신 실행 파일로 재시작한 뒤 연결 화면에서 새 로그인을 시작한다. 이전 브라우저 오류 페이지를 새로고침하는 것만으로는 요청이 바뀌지 않는다.

등록 시 선택한 비용 유형까지 허용 범위를 넓힌다. 기본 범위는 로컬·무료·구독이며, 유료 API를 등록하면 유료도 허용된다. 더 낮은 상한을 적용하려면 `loom provider policy free` 또는 `/provider policy free`를 사용한다. API 키가 있다는 이유만으로 무료라고 분류하지 않는다. 알려지지 않은 실제 단가와 사용량은 추측하지 않는다.

## 배포와 설치

```sh
# 단일 실행 파일과 체크섬을 포함한 설치용 압축 파일 생성
python3 scripts/package-loom.py

# 빌드 후 사용자 경로에 설치
python3 scripts/package-loom.py --install --bin-dir ~/.cargo/bin

loom --cwd /path/to/git/repository
```

패키지 안의 `loom` 하나로 TUI, 코어 CLI, 공급자 관리, 모델 다운로드와 로컬 추론을 실행한다. 선택 기능인 Codex 호환 모드는 Codex 설치가 따로 필요하다. 코딩 작업의 작업 폴더는 Git 저장소 루트를 지정한다.

**환경 → 추론 엔진** 또는 `/runtime`에서 체크포인트 다운로드, 진행률, 취소·이어받기, 설치 목록·용량·선택·삭제를 관리한다. 다운로드는 별도로 실행되어 대화를 막지 않는다. 삭제는 파일 이름을 확인한 뒤 확정하며, 작업 중에는 막는다. Hugging Face의 공개 GGUF 주소를 사용한다. 같은 주소로 다시 다운로드하면 받은 부분을 검증하고 이어받는다.

로컬 추론은 프로세스 내부의 GGUF 엔진을 사용하며 현재 **Spark2.5 아키텍처**를 지원한다. CPU와 macOS Metal을 화면에서 선택하면 다음 실행에도 기억한다. `LOOM_INFERENCE_BACKEND=auto|cpu|mps|cuda`는 저장한 설정보다 우선한다. CUDA 호스트에서는 `--cuda`로 패키징한다. 현재 저수준 추론 백엔드는 llama.cpp 바인딩이며, 별도 Ollama나 llama-server를 띄울 필요가 없다. 완전히 새로 작성한 텐서 엔진은 아니다.

```sh
loom model pull hf:owner/repo/file.gguf
loom model list
loom model remove owner/repo/file.gguf
loom runtime backend cpu
LOOM_INFERENCE_BACKEND=cpu loom
LOOM_INFERENCE_BACKEND=mps loom
python3 scripts/package-loom.py --cuda
```

## 작업 중 대화와 질문

모델이 도구 호출과 함께 보낸 중간 설명은 도구 기록 사이에 일반 대화로 남는다. 중간 설명을 위해 별도 모델 호출을 만들지는 않는다. 작업 상세에는 실제 응답 모델을 표시하고, 모델 정보가 없으면 중립적으로 표시한다.

대화 행의 분기 버튼과 전용 여백을 제거해 본문 폭을 확보했다. 분기가 필요하면 `/branch [N]` 명령을 사용한다.

질문 폼의 **취소** 또는 `Ctrl+X`는 그 질문만 취소하며, 모델에는 사용자가 답을 선택하지 않고 취소했다고 전달한다. `Tab`과 `Shift+Tab`으로 이전·다음·제출·나중에·취소 버튼을 이동한다. 진행 중인 작업 전체 중단은 `Esc`다. 공급자 등록과 모델 다운로드 입력은 로컬 폼이므로 취소나 입력 내용을 모델에 보내지 않는다.

`Esc`를 누르면 즉시 **중단됨**으로 표시하고 모델 연결과 실행 중인 명령을 취소한다. 입력 중인 초안은 보존한다. 저장소 확인·작업 공간 생성·인덱싱도 취소를 확인하며, 이미 수정한 내용이 있으면 복구 기록을 남긴다. 작업을 시작할 때는 **대화 기록 불러오는 중 → 저장소 상태 확인 중 → 작업 공간 준비 중 → 코드 인덱스 확인 중**으로 실제 단계를 표시한다. 대화 기록 로딩은 입력을 막지 않으며, 새 작업 공간을 다시 캡처하는 중복 처리를 제거했다.

`/permissions`에서 **승인 요청 안 함**을 선택한 뒤 **변경 확인**으로 확정하면 해당 작업 폴더의 설정을 다음 실행에도 기억한다. 이 설정에서는 검증을 통과한 변경을 승인 창 없이 적용한다. 읽기 전용·실행 권한·변경 범위·원본 충돌 검사는 그대로 적용하며, 진행 중인 작업이 끝난 뒤 설정을 변경할 수 있다.

## 확인된 범위

API 키 저장·다중 계정·인증 헤더·직접 호출·TUI 등록·연결 갱신·삭제는 로컬 모의 서버와 실제 터미널에서 검증한다. OAuth는 상태/nonce/서명/권한/토큰 갱신/저장 검증을 모의 테스트로 확인한다. 실제 계정의 승인·사용 가능 모델·요금제 사용은 로그인한 사용자의 계정에서 확인해야 한다. Windows OAuth는 별도의 보호된 자격증명 저장소가 구현되기 전까지 지원하지 않는다. CUDA 실추론은 CUDA 장비에서 검증해야 한다.

ChatGPT 로그인은 [공식 등록·로그인 문서](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)의 공개 클라이언트 흐름과 [모델·추론 문서](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)를 따른다.
