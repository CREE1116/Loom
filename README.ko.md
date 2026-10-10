# Loom

[English](README.md) · [한국어](README.ko.md)

Loom은 터미널 코딩 에이전트입니다. 저장소의 격리된 사본에서 작업하고, 변경은 승인을 받은
뒤에 적용합니다. 원격 모델 공급자와 로컬 모델을 함께 쓸 수 있습니다. 로컬 모델은 별도
프로세스 없이 Loom 안에서 실행됩니다: GGUF는 내장 llama.cpp로, Apple silicon에서는 MLX로,
그리고 Loom의 자체 형식인 LMX로 실행합니다.

**상태: 초기 단계.** 주 사용 환경은 macOS(Apple silicon)입니다. Linux·Windows는 CI에서
빌드와 테스트를 통과하며, CUDA 빌드는 실제 하드웨어에서 아직 검증하지 않았습니다.

## 설치

명령 한 줄로 설치합니다. Apple silicon 맥은 릴리즈 바이너리를 받고, 다른 시스템은 아래
준비물로 소스에서 빌드합니다.

```sh
curl -fsSL https://raw.githubusercontent.com/CREE1116/Loom/main/scripts/get.sh | sh
```

저장소가 비공개인 동안은 GitHub CLI로 받습니다:

```sh
gh api repos/CREE1116/Loom/contents/scripts/get.sh -H "Accept: application/vnd.github.raw" | sh
```

옵션: `LOOM_PREFIX`(기본 `~/.local`), `LOOM_VERSION`, `LOOM_FROM_SOURCE=1`, `LOOM_CUDA=1`.
다시 실행하면 업데이트됩니다.

### 소스에서

필요한 것: [Rust](https://rustup.rs), git, CMake, C/C++ 컴파일러(macOS는 Xcode Command
Line Tools, Linux는 `build-essential`, Windows는 Visual Studio Build Tools). Apple
silicon에서는 MLX 런타임을 받기 위해 `python3`를 사용합니다.

```sh
git clone https://github.com/CREE1116/Loom.git
cd Loom
scripts/install.sh            # ~/.local/bin/loom 에 설치
```

옵션: `--prefix DIR`, `--cuda`(NVIDIA, CUDA toolkit 필요), `--no-mlx`.

Windows(PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install.ps1   # -Prefix DIR, -Cuda
```

첫 빌드는 llama.cpp를 컴파일하느라 몇 분 걸립니다. 모델·공급자 설정·키 같은 사용자
데이터는 `~/.loom`에 있으며 재설치·삭제해도 유지됩니다. 삭제하려면 `loom` 실행 파일(macOS는
`~/.local/lib/loom`도)을 지우면 됩니다.

## 시작하기

```sh
loom --cwd /path/to/your/repository
loom --demo                   # 모델 없이 예시 데이터로 화면 확인
```

Loom 안에서:

- `/provider` — 공급자 연결(OpenAI, Anthropic, Gemini, Groq, OpenRouter, DeepSeek,
  OpenAI 호환 서버) 또는 ChatGPT 로그인.
- `/model` — 모델 선택. **auto**로 두면 `/routing`에서 정한 비용 상한 안에서 비용과
  가용성을 보고 Loom이 고릅니다.
- `/runtime` — 로컬 모델 다운로드(`hf:소유자/저장소` 또는 GGUF 링크), 추론 장치 선택,
  모델 삭제.
- `/help` — 전체 명령.

요청을 입력하면 Loom이 코드를 읽고, 격리된 사본에서 변경하고, 검사를 실행한 뒤 diff를
보여줍니다. 승인하기 전까지 작업 폴더는 바뀌지 않습니다.

## 로컬 모델

- 내려받은 GGUF는 기본적으로 Loom의 단일 파일 모델 형식인 **LMX**로 변환합니다.
  `/runtime`에서 "받은 GGUF를 LMX로 변환", "변환 후 원본 GGUF 보관"을 켜고 끌 수 있습니다.
  `/model`에는 모델마다 한 항목만 보입니다.
- 네이티브 엔진이 지원하는 모델은 Loom 자체 엔진에서 실행합니다: Qwen3.5/3.6(dense·MoE),
  LFM2/LFM2.5(dense·MoE) GGUF. MoE 모델은 토큰이 쓰는 expert만 메모리에 올리고, 메모리
  예산은 RAM 크기로 정하며, 다른 프로그램이 메모리를 필요로 하면 돌려줍니다. 다른 모델은
  llama.cpp(Metal, CUDA, CPU)나 MLX로 실행합니다.
- 모든 세션이 공유하는 프롬프트 앞부분(시스템 프롬프트와 도구 정의)은 디스크에 캐시해서,
  새 세션의 첫 응답에서 다시 계산하지 않습니다.

명령줄:

```sh
loom model list
loom model pull hf:owner/repo[/file.gguf]
loom model import model.gguf                           # GGUF를 감싼 LMX 파일
loom model import model.gguf --native --output m.lmx   # 네이티브 LMX(v2)
loom model inspect lmx/<id> --verify
loom model bench <id> --prompt 512 --gen 64
```

## 긴 대화

이전 대화가 모델 컨텍스트(로컬 모델은 실제 크기)를 넘기 시작하면 다음 요청 전에 Loom이
자동으로 요약하고, 그동안 "컨텍스트 압축 중"을 표시합니다. `/compact`로 직접 요약할 수도
있습니다.

## 스킬

스킬은 `SKILL.md`(머리말에 `name`, `description`, 그 아래 지침)가 든 폴더입니다. 저장소와
홈 디렉터리의 `.loom/skills`, `.agents/skills`, `.codex/skills`, `.claude/skills`, 그리고
Claude Code·Codex 플러그인 캐시의 스킬을 읽습니다. 요청에 `$이름`으로 언급하거나 `/skills`에서
고르면 되고, 모델이 필요할 때 직접 불러오기도 합니다.

기본 제공 스킬: `loom-guide`(Loom 구조와 흔한 문제 해결), `skill-builder`(스킬 작성법),
`loom-extension`(도구 추가 방법).

## 확장

`<저장소>/.loom/config.toml`로 도구를 추가합니다: MCP 서버(stdio 명령 또는 HTTP URL)와
웹 검색(SearxNG 또는 Brave).

```toml
[mcp_servers.github]
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_TOKEN = "..." }

[web_search]
provider = "searxng"
endpoint = "https://search.example/search"
```

## 파일 위치

| | |
| --- | --- |
| 모델 | `~/.loom/models/` (`$LOOM_MODELS`로 변경) |
| 공급자·키 | `~/.loom/providers.toml`, `~/.loom/provider-credentials.json` (권한 0600) |
| 요청 기록 | `~/.loom/model-usage.jsonl` |
| 저장소별 상태 | `<저장소>/.loom/`, UI 설정은 `<저장소>/.custom-tui/` |

## 저장소 구성

| 경로 | |
| --- | --- |
| `loom-core/` | 에이전트 루프, 도구, 작업 공간 격리, 라우팅, 공급자, 로컬 추론, LMX |
| `loom-core/native/lmx/` | 네이티브 LMX 엔진(ggml 기반 C++) |
| `loom-tui/` | 터미널 UI와 `loom` 실행 파일 |
| `scripts/` | 설치·패키징·벤치마크 도구 |
| `docs/` | 설계 문서와 측정 기록 |

## 개발

```sh
cargo test --manifest-path loom-core/Cargo.toml
cargo test --manifest-path loom-tui/Cargo.toml
cargo run --manifest-path loom-tui/Cargo.toml -- --self-check   # 화면 시나리오 검사
scripts/loom --cwd .                                              # 소스에서 실행
```

UI를 바꿀 때는 self-check 시나리오가 필요합니다. `loom-tui/AGENTS.md`를 참고하세요.

## 라이선스

Apache License 2.0. [LICENSE](LICENSE)와 [NOTICE](NOTICE)를 참고하세요. 개발은 OpenAI
Codex 소스 checkout에서 시작했으며, Loom은 Codex에 의존하거나 Codex를 포함하지 않습니다.
