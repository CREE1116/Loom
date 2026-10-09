"""Direct provider lifecycle and actual TUI registration; localhost fixtures only."""
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from terminal_smoke import BINARY, Session


class Fixture(BaseHTTPRequestHandler):
    calls = []
    extra_catalog_model = False
    hold_request = False
    waiting = threading.Event()
    disconnected = threading.Event()
    def log_message(self, *_):
        pass
    def do_GET(self):
        self.calls.append((self.path, self.headers.get("Authorization"), self.headers.get("x-api-key")))
        if self.path.endswith("/models/user"):
            models = []
            for i in range(421 if self.extra_catalog_model else 420):
                rate = "0" if i < 10 else ("0.0000004" if i < 210 else ("0.000004" if i < 400 else "0.00003"))
                models.append({"id": f"vendor/model-{i:03}", "architecture": {"output_modalities": ["text"]},
                    "context_length": 128000, "pricing": {"prompt": "0" if i < 10 else "0.000001", "completion": rate},
                    "supported_parameters": [] if i == 9 else ["tools"]})
            self.reply({"data": models})
            return
        self.reply({"data": [{"id": "fixture"}]})
    def reply(self, body):
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.calls.append((self.path, self.headers.get("Authorization"), self.headers.get("x-api-key")))
        assert body["model"] == "fixture", body
        assert body.get("tools"), "coding tools must reach the provider"
        if self.hold_request:
            self.waiting.set()
            self.connection.settimeout(4)
            try:
                if self.rfile.read(1) == b"":
                    self.disconnected.set()
            except (TimeoutError, OSError):
                pass
            return
        if self.path.endswith("/responses"):
            self.reply({"model": "fixture", "status": "completed", "output": [{"type": "message", "content": [{"type": "output_text", "text": "DIRECT_PROVIDER_OK"}]}]})
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        data = 'data: ' + json.dumps({"choices": [{"delta": {"content": "DIRECT_PROVIDER_OK"}, "finish_reason": None}]}) + '\n\n'
        data += 'data: ' + json.dumps({"choices": [{"delta": {}, "finish_reason": "stop"}], "model": "fixture", "usage": {"prompt_tokens": 10, "completion_tokens": 3}}) + '\n\ndata: [DONE]\n\n'
        self.send_header("Content-Length", str(len(data.encode())))
        self.end_headers()
        self.wfile.write(data.encode())


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_port}/v1"
    try:
        with tempfile.TemporaryDirectory(prefix="loom-providers-") as directory:
            root = Path(directory)
            model_state = root / "models"
            cwd = root / "repo"
            cwd.mkdir()
            subprocess.run(["git", "init", "-q", str(cwd)], check=True)
            (cwd / "fixture.txt").write_text("provider fixture\n")
            subprocess.run(["git", "add", "."], cwd=cwd, check=True)
            subprocess.run(["git", "-c", "user.name=Loom Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"], cwd=cwd, check=True)
            env = {**os.environ, "LOOM_MODEL_STATE_DIR": str(model_state), "LOOM_SETTINGS_STATE_DIR": str(root / "settings"), "LOOM_OMNIROUTE": "0"}
            env["LOOM_MODELS"] = str(root / "checkpoints")
            env.pop("LOOM_INFERENCE_BACKEND", None)
            # This scenario asserts hover colors; keep the fixture in color mode.
            env.pop("NO_COLOR", None)
            for key in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY", "GROQ_API_KEY", "OPENROUTER_API_KEY"):
                env.pop(key, None)
            def cli(*args, key=None):
                result = subprocess.run([str(BINARY), "provider", *args], input=key, env=env, text=True, capture_output=True, timeout=15)
                assert result.returncode == 0, result.stderr
                assert "secret-provider-fixture" not in result.stdout + result.stderr
                return result.stdout
            for name, provider in [("work", "compatible"), ("other", "anthropic")]:
                cli("add", provider, "--name", name, "--base-url", base, "--model", "fixture", "--key-stdin", key="secret-provider-fixture\n")
            catalog = cli("add", "openrouter", "--name", "router", "--base-url", base, "--key-stdin", key="secret-provider-fixture\n")
            assert "420" in catalog and "router/vendor/model-419" in catalog, catalog
            assert any(path.endswith("/models/user") and auth == "Bearer secret-provider-fixture" for path, auth, _ in Fixture.calls)
            config = model_state / "providers.toml"
            credentials = model_state / "provider-credentials.json"
            assert "secret-provider-fixture" not in config.read_text()
            assert stat.S_IMODE(credentials.stat().st_mode) == 0o600
            assert "work" in cli("list") and "other" in cli("list")
            cli("test", "work")
            cli("test", "other")
            assert any(auth == "Bearer secret-provider-fixture" for _, auth, _ in Fixture.calls)
            assert any(key == "secret-provider-fixture" for _, _, key in Fixture.calls)
            cli("disable", "other")
            session = Session("--new", "--model", "work/fixture", cwd=cwd, env=env)
            try:
                session.wait("메시지 입력…", timeout=20)
                session.send("hello fixture\r")
                try:
                    session.wait("DIRECT_PROVIDER_OK", timeout=15)
                except AssertionError:
                    session.send("/tool 1\r")
                    session.pump(1)
                    raise AssertionError(session.screen.text())
                assert any(path.endswith("/chat/completions") for path, _, _ in Fixture.calls)
                assert "secret-provider-fixture" not in session.raw.decode(errors="replace")
                Fixture.hold_request = True
                session.send("cancel an idle model response\r")
                assert Fixture.waiting.wait(3), session.screen.text()
                session.send("preserved draft")
                stopped_at = time.monotonic()
                session.send("\x1b")
                session.wait("중단됨", timeout=1)
                assert "중단 요청 중" not in session.screen.text()
                assert "preserved draft" in session.screen.text()
                assert Fixture.disconnected.wait(.5), "Esc left inference running on an idle socket"
                assert time.monotonic() - stopped_at < .75
                Fixture.hold_request = False
                calls_before_resume = sum(path.endswith("/chat/completions") for path, _, _ in Fixture.calls)
                session.send("\r")
                deadline = time.monotonic() + 5
                while sum(path.endswith("/chat/completions") for path, _, _ in Fixture.calls) == calls_before_resume and time.monotonic() < deadline:
                    session.pump()
                assert sum(path.endswith("/chat/completions") for path, _, _ in Fixture.calls) > calls_before_resume, session.screen.text()
                session.wait("DIRECT_PROVIDER_OK", timeout=5)
                session.wait("✓ 완료", timeout=5)
                session.resize(140, 60)
                session.command("/permissions")
                session.click("승인 요청 안 함")
                session.click("변경 확인")
                session.wait("● 승인 요청 안 함")
                assert "V2/V3" not in session.screen.text()
                session.click("필요시 사용자 승인 요청")
                session.wait("● 필요시 사용자 승인 요청")
                # Opening /model must discover models added after Loom started.
                Fixture.extra_catalog_model = True
                session.command("/model")
                session.click("모델 이름·공급자 입력")
                session.send("router model-420")
                session.wait("router/vendor/model-420", timeout=5)
                session.click("검색 지우기")
                session.wait("가격별")
                session.click("모델 이름·공급자 입력")
                session.send("ROUTER model-418")
                session.wait("router/vendor/model-418")
                session.wait("입력 $1 · 출력 $30")
                assert "model-001" not in session.screen.text()
                session.click("검색 지우기")
                session.click("무료만")
                session.wait("무료 API · 10")
                assert "고가" not in session.screen.text()
                session.click("무료만")
                session.command("/provider")
                session.wait("공급자 연결")
                session.send("/provider add\r")
                session.wait("연결할 공급자")
                # Choose compatible (last of the API templates), then fill the form.
                for _ in range(6):
                    session.send("\x1b[B")
                session.send("\r")
                for value in ("ui", base, "secret-provider-fixture"):
                    session.send(value)
                    if value == "secret-provider-fixture":
                        assert value not in session.screen.text()
                    session.send("\r")
                session.send("\r")  # automatic routing
                session.send("\r")  # paid API
                session.wait("> ui/", timeout=15)
                session.wait("ui/fixture", timeout=15)
                session.pump(1)
                assert "ui" in config.read_text()
                assert "secret-provider-fixture" not in session.raw.decode(errors="replace")
                session.command("/model ui/fixture")
                session.command("/chat")
                before_calls = sum(path.endswith("/chat/completions") for path, _, _ in Fixture.calls)
                session.send("hello newly registered provider\r")
                session.wait("hello newly registered provider")
                deadline = time.monotonic() + 15
                while sum(path.endswith("/chat/completions") for path, _, _ in Fixture.calls) == before_calls and time.monotonic() < deadline:
                    session.pump()
                assert sum(path.endswith("/chat/completions") for path, _, _ in Fixture.calls) > before_calls, session.screen.text()
                session.wait("DIRECT_PROVIDER_OK", timeout=15)
                session.command("/routing")
                session.wait("자동 라우팅 설정")
                session.wait("비용 상한")
                session.command("/runtime")
                session.wait("로컬 추론 엔진")
                session.click("CPU")
                assert 'inference_backend = "cpu"' in config.read_text()
                checkpoint = root / "checkpoints" / "fixture.gguf"
                checkpoint.parent.mkdir(exist_ok=True)
                checkpoint.write_bytes(b"managed deletion fixture")
                session.command("/runtime refresh")
                session.wait("fixture.gguf")
                session.click("모델 삭제")
                assert checkpoint.exists(), "delete requires choosing the concrete file"
                session.click("삭제 확인")
                assert not checkpoint.exists()
                session.click("+ 모델 다운로드")
                session.wait("Hugging Face GGUF 주소")
                x, y = session.screen.position("다음")
                before = session.screen.backgrounds[y][x]
                session.send(f"\x1b[<35;{x+1};{y+1}M")
                deadline = time.monotonic() + 3
                while session.screen.backgrounds[y][x] == before and time.monotonic() < deadline:
                    session.pump()
                assert session.screen.backgrounds[y][x] != before, f"question Next must react to mouse-over: {before} -> {session.screen.backgrounds[y][x]} at {x},{y}\n{session.screen.text()}"
                session.send("\x18")  # Ctrl+X cancels just the local input form.
                session.wait("+ 모델 다운로드")
                assert "Hugging Face GGUF 주소" not in session.screen.text()
                session.command("/provider remove ui")
                assert 'id = "ui"' not in config.read_text()
            finally:
                session.close()
            cli("remove", "work")
            cli("enable", "other")
            cli("remove", "other")
            cli("remove", "router")
            assert not json.loads(credentials.read_text())
            print("Provider CLI + PTY: idle inference cancellation/resume, approval policy controls, 420-model search/prices/free filter, multi-account/direct inference/live reload, question hover/cancel, routing and checkpoint management OK", flush=True)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
