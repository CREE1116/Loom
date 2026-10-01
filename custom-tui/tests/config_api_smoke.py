"""Verify Codex trust persistence against an isolated temporary CODEX_HOME.

Never edits the user's real Codex configuration or starts a model turn.
"""
import json
import os
import select
import subprocess
import tempfile
import time
import tomllib
from pathlib import Path


def send(server, payload):
    server.stdin.write(json.dumps(payload, ensure_ascii=False) + "\n")
    server.stdin.flush()


def reply(server, target, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not select.select([server.stdout], [], [], .2)[0]:
            continue
        line = server.stdout.readline()
        if not line:
            raise AssertionError("Codex app-server ended before replying")
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("id") == target:
            if "error" in message:
                raise AssertionError(message["error"])
            return message["result"]
    raise AssertionError(f"Codex app-server did not answer request {target}")


def main():
    with tempfile.TemporaryDirectory(prefix="custom-tui-config-api-") as directory:
        config_home = Path(directory)
        config_file = config_home / "config.toml"
        config_file.write_text("", encoding="utf-8")
        server = subprocess.Popen(
            [os.environ.get("CODEX_BINARY", "codex"), "app-server", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env={**os.environ, "CODEX_HOME": directory},
        )
        try:
            send(server, {"id": 1, "method": "initialize", "params": {
                "clientInfo": {"name": "custom-tui-config-smoke", "version": "0.1.0"},
                "capabilities": {"experimentalApi": True},
            }})
            reply(server, 1)
            send(server, {"method": "initialized"})
            send(server, {"id": 2, "method": "config/value/write", "params": {
                "keyPath": 'projects."/tmp/example.project".trust_level',
                "mergeStrategy": "upsert",
                "value": "trusted",
            }})
            reply(server, 2)
            actual = tomllib.loads(config_file.read_text(encoding="utf-8"))
            assert actual["projects"]["/tmp/example.project"]["trust_level"] == "trusted"
            print("Isolated Codex config API: project trust write and parse OK", flush=True)
        finally:
            server.terminate()
            server.wait(timeout=4)


if __name__ == "__main__":
    main()
