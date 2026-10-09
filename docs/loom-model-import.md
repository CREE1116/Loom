# 모델 준비: 첫 MLX 가져오기 경로

2026-10-09. 전체 엔진 재설계의 단계 B 첫 구현 기록이다. 현재 제품 CLI의 최종 저장 결과는 단일 [LMX v1](loom-lmx.md)으로 변경했다. 아래 폴더 게시·측정 기록은 유지 중인 Rust MLX directory import API와 LMX 도입 전 검증을 설명한다. Windows·Linux/CPU/CUDA 작업은 모델 준비와 병행하며 이미지 입력은 별도 단계다.

## 사용

```sh
loom model import /absolute/path/to/mlx-checkpoint --json
loom model pull mlx:mlx-community/Qwen3-4B-4bit
loom model pull mlx:owner/repo@commit-or-tag
loom model list
loom model inspect lmx/<identity> --verify --json
loom model bench lmx/<identity> --engine mlx --prompt 128 --gen 32 --json
```

`hf:owner/repo[/file.gguf]`와 `mlx:` 다운로드는 검증된 원본을 보관하고 LMX도 만든다. `ollama:name`은 Ollama 저장소를 사용한다. `model serve`는 GGUF용이며, MLX 모델은 Loom 내부 엔진에서 실행한다. `model remove`는 아직 GGUF 파일용이다.

## 준비 과정

- 로컬 가져오기는 원본과 별도의 관리 사본을 만든다. 전체 모델을 읽어 들이거나 FP16으로 확장하지 않고 1 MiB 버퍼로 복사·SHA-256 검증한다.
- Hugging Face는 요청 revision을 한 번 commit SHA로 고정한 뒤 모든 파일을 그 SHA에서 받는다. 기존 다운로드의 파일 잠금, 취소, `.part` 이어받기, LFS SHA-256 검사, 파일 게시를 재사용한다. 지원되지 않는 config는 가중치 다운로드 전에 거부한다. shard index가 있으면 필요한 weight shard만 받는다.
- staging은 `LOOM_MODELS/.imports`에 둔다. 검증된 폴더를 `LOOM_MODELS/mlx/imported/<identity>`에 한 번의 rename으로 게시한다. 검증 전 폴더는 MLX discovery가 보지 못한다.
- 로컬 취소는 임시 사본을 정리한다. HF 취소는 같은 commit의 `.part`를 남겨 재개한다. 원본은 삭제하지 않는다.
- 같은 출처·원본 파일 해시·준비 계획은 동일 identity를 사용한다. 캐시 재사용 전에 전체 파일 해시·구조·manifest를 재검사한다. HF의 준비된 사본이 있으면 payload를 다시 다운로드하지 않는다. 로컬 재가져오기는 현재 임시 복사·검증을 거친 뒤 기존 사본을 재사용한다.

`loom-model.json`에는 준비 계획 버전, 출처·revision, 전체 파일 길이·SHA-256, config, 텐서 이름·dtype·shape·payload 위치·byte 범위를 기록한다. packed affine weight는 bits·group size·축·packing·scale/bias 텐서 참조를 기록한다. tokenizer·template·LICENSE·README도 존재하면 원본 바이트와 해시를 보존한다. upstream LFS SHA-256이 없는 작은 파일은 고정 revision에서 받은 뒤 로컬 SHA-256을 기록한다.

## 검사와 현재 범위

지원 대상은 현재 자체 MLX의 Qwen2/Qwen3/Qwen3 MoE/Llama/Mistral 호환 구조다. FP16/BF16/FP32와 MLX U32 affine packing을 검사하며, 기존 2/3/4/5/6/8bit 표현을 새 형식으로 재양자화하지 않는다. 실제 정밀도는 packing에서 추론한다. 모델 내 group size는 현재 동일해야 한다. MoE는 이미 packed `switch_mlp` expert bank가 있는 체크포인트를 받는다.

검사 항목은 전체 shard/index 일치, 중복 텐서, 파일 truncation, tensor shape/offset/byte 수, 겹침·빈 구간·미색인 데이터, 필수 projection/norm/expert shape, affine packing, tokenizer와 chat template다. tokenizer의 실제 로딩 검사는 MLX가 포함된 Apple 빌드에서 수행한다. 다른 빌드는 tokenizer JSON 구조를 검사하고 실행 backend가 없다는 사실을 결과에 표시한다.

scaled RoPE·partial RoPE·sliding window·shared expert·다른 activation·GPTQ/AWQ/MXFP/NVFP 등 미지원 조합은 거부한다. 폴더에 임의 코드를 포함해 실행하지 않는다. 선택된 payload는 일반 파일이어야 하며 symlink를 받지 않고 Unix에서는 hard link도 거부한다.

준비 작업은 공통 resource governor에서 256 MiB를 예약한다. 복사 버퍼는 1 MiB, 개별 tensor header는 8 MiB, 개별 metadata/manifest는 32 MiB, 텐서는 최대 50,000개다. 예약은 OS 강제 메모리 한도가 아니다. 대기·취소·오류에서도 lease를 반환한다. staging과 최종 모델은 같은 관리 root에 있어 같은 파일시스템 rename을 사용한다.

이 검사는 구조와 바이트 보존을 증명한다. 모든 모델의 실제 추론이나 품질을 인증하지 않으며, manifest에도 이 구분을 기록한다. 실행 전 매번 전체 가중치 SHA-256을 다시 계산하는 기능은 추가하지 않았다. 외부에서 관리 사본을 바꿨다면 다시 가져올 때 재검증한다.

## 검증 기록

- Core 전체 회귀: 146 passed, 7 ignored. 자원 관리 통합: 6 passed.
- 최종 가져오기 검사 5개: 바이트 보존과 tamper 감지, 미완성 shard·미지원 config 거부, 복사/게시 취소와 재시도, symlink 경계, HTTP 고정 revision·중단 재개·payload 재다운로드 방지.
- 모델 metadata 검사에는 packed MoE expert bank와 8bit router/4bit expert 혼합 fixture를 포함한다. 실제 30B MoE 검증을 의미하지 않는다.
- 제품 전체 테스트: 324 passed, 3 ignored.
- 최종 release 빌드와 자가검사: 785 passed, 0 failed.
- 실제 `mlx-community/Qwen3-4B-4bit`: 2,274,455,826 bytes, 904 tensors, packed affine weight 253개를 보존해 가져왔다. 원본과 준비 사본의 128-token 입력 뒤 생성한 32개 토큰이 전부 일치했다.
- 최종 바이너리의 가져오기 프로세스 maximum RSS 154,910,720 bytes, peak physical footprint 119,014,000 bytes. OS 파일 캐시나 작업 전체 메모리 측정이 아니다. 기준 원본 추론은 제품 테스트와 겹쳤고 최종 사본 검사는 최종 빌드 뒤 수행했으므로 속도 비교로 사용하지 않는다.
- 실제 HF API 응답의 commit/file/LFS metadata 형식도 확인했다. HTTP 다운로드의 end-to-end 검증은 작은 로컬 서버 fixture이며, 실제 HF에서 4B 모델을 새로 전송하는 검사는 수행하지 않았다.

실제 모델 결과는 [측정 JSON](../bench/mlx-memory-moe/model-import.json)에 둔다. 검증용 관리 사본은 임시 root에서 만들었다. 기존 사용자 모델 폴더는 수정하지 않았다.

## 다음 작업

1. GGUF scheme별 정밀도 보존 가능성을 확인하고 지원 가능한 GGUF→MLX 경로 추가. 미지원 조합은 기존 GGUF backend 유지.
2. 변환 계획·검증을 준비 파이프라인에 연결. 전체 FP16 확장 없이 블록 단위로 처리하고, 재양자화는 명시적인 별도 profile로 구분.
3. per-tensor group size와 양자화 형식, 모델 연산 지원 확대. 동시 세션 구현을 앞세우지 않는다.

형식의 기준은 [Safetensors 공식 형식](https://huggingface.co/docs/safetensors/index), revision/file metadata의 기준은 [Hugging Face Hub API](https://huggingface.co/docs/huggingface_hub/en/package_reference/hf_api)다.
