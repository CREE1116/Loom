"""V5 process-start disclosure and /plan approval through the actual terminal."""
import json
import subprocess
import tempfile
import time
from pathlib import Path
from terminal_smoke import Session


def response(*calls):
    return {"parts": [{"ToolCall": {"id": f"c{i}", "name": name, "arguments": args}}
                      for i, (name, args) in enumerate(calls)],
            "usage": {"input": 10, "cached_input": 0, "output": 5},
            "stop": "ToolUse", "model": "scripted", "latency": {"secs": 0, "nanos": 0}}


def plan_approval():
    with tempfile.TemporaryDirectory(prefix="loom-v5-pty-") as directory, tempfile.NamedTemporaryFile(mode="w", suffix=".json") as script:
        root = Path(directory)
        subprocess.run(["git", "init", "-q", directory], check=True)
        (root / "fixture.txt").write_text("original")
        json.dump([
            response(("agent", {"units": [{"id": "a", "goal": "파일 생성", "write_files": ["new.txt"], "depends_on": []}]})),
            response(("change", {"op": "create", "target": "new.txt", "content": "approved V5"}), ("finish", {"summary": "자식 작업 완료"})),
            response(("finish", {"summary": "V5 계획 수행 완료"})),
        ], script)
        script.flush()
        session = Session("--backend", "v5", "--native-sandbox", "full-access", "--core-check", "git diff --check", "--script", script.name, cwd=root)
        try:
            session.wait("메시지 입력…")
            session.send("/plan 파일을 생성해줘\r")
            session.wait("V5 검증된 변경 승인", timeout=15)
            assert not (root / "new.txt").exists(), "proposal escaped before approval"
            session.send("\r")
            deadline = time.monotonic() + 10
            while not (root / "new.txt").exists() and time.monotonic() < deadline:
                session.pump()
            assert (root / "new.txt").read_text() == "approved V5", session.screen.text()
            session.wait("메시지 입력…")
        finally:
            session.close()
    print("PTY V5: /plan -> isolated DAG -> keyboard approval -> apply OK", flush=True)


def command_started_before_return():
    with tempfile.TemporaryDirectory(prefix="loom-v5-command-pty-") as directory, tempfile.NamedTemporaryFile(mode="w", suffix=".json") as script:
        root = Path(directory)
        subprocess.run(["git", "init", "-q", directory], check=True)
        (root / "fixture.txt").write_text("original")
        json.dump([
            response(("run", {"kind": "command", "command": "sleep 2"})),
            response(("finish", {"summary": "명령 확인 완료"})),
        ], script)
        script.flush()
        session = Session("--backend", "v5", "--native-sandbox", "full-access", "--script", script.name, cwd=root)
        try:
            session.wait("메시지 입력…")
            session.send("명령을 실행해줘\r")
            session.wait("명령 실행 중", timeout=8)
            assert "sleep 2" in session.screen.text(), session.screen.text()
            command_rows = ["".join(row) for row in session.screen.cells if "sleep 2" in "".join(row)]
            assert len(command_rows) == 1, session.screen.text()
            assert command_rows[0].count("실행 중") == 1, command_rows
            assert any(c in command_rows[0] for c in "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"), command_rows
            session.wait("명령 확인 완료", timeout=8)
            assert "작업 기록" in session.screen.text(), session.screen.text()
            assert "✓" in session.screen.text(), session.screen.text()
        finally:
            session.close()
    print("PTY V5: running command visible before return, completion distinct OK", flush=True)


if __name__ == "__main__":
    plan_approval()
    command_started_before_return()
