"""Exercise the actual binary through a PTY. Default tests never call a model.

Run with --live to additionally send one short, tool-free inference request,
read it from a second client, and resume it after closing the first window.
"""
import argparse
import codecs
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import termios
import time
import tempfile
import unicodedata

ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get("CUSTOM_TUI_BINARY", ROOT / "custom-tui/target/debug/custom-tui"))


class Screen:
    """Decode the renderer's cursor/color stream, including split UTF-8/CSI."""
    def __init__(self, width=80, height=24):
        self.width, self.height = width, height
        self.cells = [[" " for _ in range(width)] for _ in range(height)]
        self.x = self.y = 0
        self.buffer = ""
        self.bg = None
        self.fg = None
        self.backgrounds = [[None for _ in range(width)] for _ in range(height)]
        self.foregrounds = [[None for _ in range(width)] for _ in range(height)]
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        self.buffer += self.decoder.decode(data)
        i = 0
        while i < len(self.buffer):
            ch = self.buffer[i]
            if ch == "\x1b":
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.buffer[i:])
                if not match:
                    break
                args, _, command = match.groups()
                if command in "Hf":
                    parts = args.split(";")
                    self.y = max(0, int(parts[0] or 1) - 1)
                    self.x = max(0, int(parts[1] or 1) - 1) if len(parts) > 1 else 0
                if command == "m":
                    parts = [int(p or 0) for p in args.split(";")]
                    if parts[:2] == [48, 2] and len(parts) >= 5:
                        self.bg = tuple(parts[2:5])
                    elif 0 in parts or 49 in parts:
                        self.bg = None
                    if parts[:2] == [38, 2] and len(parts) >= 5:
                        self.fg = tuple(parts[2:5])
                    elif 0 in parts or 39 in parts:
                        self.fg = None
                i += match.end()
                continue
            if ch == "\r":
                self.x = 0
            elif ch == "\n":
                self.y += 1
            elif not unicodedata.category(ch).startswith("C"):
                width = 0 if unicodedata.combining(ch) else (2 if unicodedata.east_asian_width(ch) in "WF" else 1)
                if width and self.y < self.height and self.x < self.width:
                    self.cells[self.y][self.x] = ch
                    self.backgrounds[self.y][self.x] = self.bg
                    self.foregrounds[self.y][self.x] = self.fg
                    if width == 2 and self.x + 1 < self.width:
                        self.cells[self.y][self.x + 1] = ""
                    self.x += width
                elif width == 0 and self.y < self.height and 0 < self.x <= self.width:
                    self.cells[self.y][self.x - 1] += ch
            i += 1
        self.buffer = self.buffer[i:]

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.cells)

    def position(self, text):
        for y, row in enumerate(self.cells):
            for x in range(self.width):
                if "".join(row[x:]).startswith(text):
                    return x, y
        raise AssertionError(f"Not visible: {text}\n{self.text()}")


class Session:
    def __init__(self, *args, cwd=None, env=None, show_popup=False):
        self.master, self.slave = pty.openpty()
        self.screen = Screen()
        self.resize(80, 24)
        self.before = termios.tcgetattr(self.slave)
        self.process = subprocess.Popen([str(BINARY), "--cwd", str(cwd or ROOT), *args], stdin=self.slave, stdout=self.slave, stderr=self.slave, start_new_session=True, env={**{k:v for k,v in os.environ.items() if k != "NO_COLOR"}, "TERM": "xterm-256color", **(env or {})})
        if "--demo" in args and not show_popup:
            self.wait("승인 필요")
            self.click("나중에")

    def resize(self, width, height):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.screen = Screen(width, height)
        if hasattr(self, "process"):
            import signal
            os.kill(self.process.pid, signal.SIGWINCH)

    def pump(self, duration=.2):
        end = time.monotonic() + duration
        while time.monotonic() < end:
            readable, _, _ = select.select([self.master], [], [], .025)
            if readable:
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    return
                self.screen.feed(data)

    def send(self, text):
        os.write(self.master, text.encode())
        self.pump()

    def wait(self, text, timeout=6):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump()
            if text in self.screen.text():
                return
            if self.process.poll() is not None:
                break
        raise AssertionError(f"Did not render {text!r}\n{self.screen.text()}")

    def click(self, text):
        try:
            x, y = self.screen.position(f"[{text}]")
        except AssertionError:
            try:
                x, y = self.screen.position(f"[>{text}]")
            except AssertionError:
                x, y = self.screen.position(text)
        self.send(f"\x1b[<0;{x+1};{y+1}M")
        self.send(f"\x1b[<0;{x+1};{y+1}m")

    def close(self):
        if self.process.poll() is None:
            self.send("\x11")
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                self.process.wait(timeout=3)
        after = termios.tcgetattr(self.slave)
        assert after == self.before, "Terminal input mode was not restored"
        assert self.process.returncode == 0, f"TUI exited with {self.process.returncode}"
        os.close(self.master)
        os.close(self.slave)


def demo_test():
    workdir=tempfile.TemporaryDirectory(prefix="custom-tui-demo-")
    session = Session("--demo",cwd=workdir.name)
    try:
        session.wait("실제 실행 없음")
        session.send("\x1b[200~입력 초안\n두 번째 줄\x1b[201~")
        session.wait(f"[붙여넣은 내용 · {len('입력 초안' + chr(10) + '두 번째 줄')}자]")
        session.click("활동")
        session.wait("현재 세션의 에이전트")
        session.click("대화")
        session.wait("[붙여넣은 내용")
        session.click("파일 변경 1")
        session.wait("새 Rust 화면 엔진")
        dx, dy = session.screen.position("새 Rust 화면 엔진")
        assert session.screen.backgrounds[dy][dx] == (15, 48, 24), "Added diff line has no background"
        rx, ry = session.screen.position("기존 TUI 수정")
        assert session.screen.backgrounds[ry][rx] == (57, 22, 26), "Deleted diff line has no background"
        session.click("대화")
        session.click("승인")
        session.wait("파일 변경 승인")
        session.wait("이번 요청 허용")
        session.click("거절")
        session.wait("대기 중인 승인 요청이 없습니다")
        session.click("대화")
        session.wait("[붙여넣은 내용")
        session.resize(120, 40)
        session.wait("Loom")
        session.wait("[붙여넣은 내용")
        # Actual mouse motion highlights a button while the selected tab stays marked.
        x, y = session.screen.position("파일 변경 1")
        session.send(f"\x1b[<35;{x+1};{y+1}M")
        assert session.screen.backgrounds[y][x] == (55, 58, 62), "No mouse hover feedback"
        ax, ay = session.screen.position("● 대화")
        assert session.screen.backgrounds[ay][ax] == (66, 69, 73), "Active tab lost during hover"
        session.click("파일 변경 1")
        session.wait("에이전트가 보고한 파일 변경")
        session.wait("새 Rust 화면 엔진")
        assert "대화는 넓게" not in session.screen.text(), "Diff should use the full terminal width"
        session.wait("[붙여넣은 내용")
        # Entering text in the composer leaves the selected detail partition open.
        session.click("붙여넣은 내용 · 12자")
        session.send("x")
        session.wait("에이전트가 보고한 파일 변경")
        session.click("대화")
        session.wait("let message")
        cx, cy = session.screen.position("let message")
        assert session.screen.backgrounds[cy][cx] == (24, 28, 33), "Code block has no background"
        session.send("\x01")
        session.send("\x1b[3~" * 2)  # One delete for the pasted chip, one for x.
        session.send("/")
        session.wait("↑↓ 선택")
        session.send("\x1b[B\x1b[B\r")
        session.wait("설치된 스킬 1개")
        session.click("$demo-review")
        session.wait("$demo-review ×")
        session.send("\x1b[200~검토 초안\x1b[201~")
        session.wait(f"[붙여넣은 내용 · {len('검토 초안')}자]")
        session.click("모델")
        session.wait("다음 메시지에 사용할 모델")
        session.click("○ fixture-fast")
        session.wait("fixture-fast")
        session.wait("[붙여넣은 내용")
        session.click("도움말")
        session.wait("명령 안내")
        session.resize(80,24)
        session.wait("● 도움말")
        session.click("대화")
        session.wait("[붙여넣은 내용")
        session.click("설정")
        session.wait("시작 시 작업 패널")
        session.click("○ English")
        session.wait("● Settings")
        session.wait("Task panel on startup")
        session.click("○ 한국어")
        session.wait("● 설정")
        session.click("대화")
        session.click("대화는 넓게")
        session.wait("⑂")
        # Drag-to-copy clears an earlier message click highlight.
        mx, my = session.screen.position("대화는 넓게")
        assert session.screen.backgrounds[my][mx] == (66, 69, 73), "Message click was not highlighted"
        session.send(f"\x1b[<0;{mx+1};{my+1}M\x1b[<0;{mx+10};{my+1}m")
        assert session.screen.backgrounds[my][mx] != (66, 69, 73), "Copy left the message highlighted"
        session.click("대화는 넓게")
        session.click("⑂")
        session.wait("새 데모 분기")
        session.send("x")
        session.wait("[붙여넣은 내용 · 5자]x")  # Branch click preserves the draft and keyboard focus.
    finally:
        session.close()
        workdir.cleanup()
    print("PTY demo: partitions, hover, active tab, commands, skills, models, draft, diff, approval, resize, terminal restore OK", flush=True)


def approval_popup_test():
    with tempfile.TemporaryDirectory(prefix="custom-tui-approval-popup-") as directory:
        session = Session("--demo", cwd=directory, show_popup=True)
        try:
            session.wait("승인 필요")
            session.wait("이번 요청 허용")
            session.click("나중에")
            assert "승인 필요" not in session.screen.text(), "Dismissed approval card remains visible"
            session.click("승인")
            session.wait("이번 요청 허용")
            session.wait("거절")
        finally:
            session.close()
    print("PTY approval popup: visible choices, dismiss preserves pending approval OK", flush=True)


def session_menu_test():
    with tempfile.TemporaryDirectory(prefix="custom-tui-sessions-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.wait("실제 실행 없음")
            session.click("세션 ▾")
            session.wait("현재 대화")
            session.wait("최근 대화")
            session.wait("+ 새 대화")
            assert "demo-saved" not in session.screen.text(), "Raw session IDs leaked into the menu"
            # A pending approval explains why selection is temporarily blocked.
            assert "승인 요청을 처리한 후" in session.screen.text()
            session.click("승인")
            session.click("거절")
            session.click("세션 ▾")
            # The whole session row is a click target, not just a small text button.
            sx, sy = session.screen.position("예시 · 이전 화면 검토")
            session.send(f"\x1b[<0;{session.screen.width-3};{sy+1}M")
            session.wait("새 데모 대화")
            session.click("세션 ▾")
            session.wait("● 예시 · 이전 화면 검토")
            session.click("TUI 화면 검토")
            session.wait("새 데모 대화")
            session.click("세션 ▾")
            # The session selector remains available through subsequent switches.
            session.click("예시 · 이전 화면 검토")
            session.wait("새 데모 대화")
            session.click("세션 ▾")
            session.wait("● 예시 · 이전 화면 검토")
            # At desktop width the session pane is on the right. It must also
            # support clicking the full width of each row.
            session.resize(120, 40)
            session.wait("TUI 화면 검토")
            sx, sy = session.screen.position("TUI 화면 검토")
            session.send(f"\x1b[<0;{session.screen.width-3};{sy+1}M")
            session.wait("새 데모 대화")
            session.click("세션 ▾")
            session.wait("예시 · 이전 화면 검토")
            session.click("+ 새 대화")
            session.wait("새 데모 대화")
            session.click("세션 ▾")
            session.wait("● 새 대화")
            session.send("/sessions\r")
            session.wait("1. ○ TUI 화면 검토")
            session.wait("2. ○ 예시 · 이전 화면 검토")
            session.send("/sessions 2\r")
            session.wait("새 데모 대화")
            session.click("세션 ▾")
            session.wait("● 예시 · 이전 화면 검토")
        finally:
            session.close()
    print("PTY sessions: current, recent, switch, new and clean labels OK", flush=True)


def paging_test():
    workdir = tempfile.TemporaryDirectory(prefix="custom-tui-paging-")
    session = Session("--demo", cwd=workdir.name)
    try:
        session.wait("실제 실행 없음")
        session.resize(120, 50)
        session.wait("TUI 구조 읽기")
        session.click("TUI 구조 읽기")
        session.wait("다음 24줄")
        session.click("다음 24줄")
        session.wait("예시 출력 47")
        session.click("이전 24줄")
        session.wait("예시 출력 23")
    finally:
        session.close()
        workdir.cleanup()
    print("PTY paging: bounded output, next/previous page, terminal restore OK", flush=True)


def queue_test():
    workdir = tempfile.TemporaryDirectory(prefix="custom-tui-queue-")
    session = Session("--demo", cwd=workdir.name)
    try:
        session.wait("실제 실행 없음")
        session.click("승인")
        session.wait("파일 변경 승인")
        session.wait("이번 요청 허용")
        session.click("거절")
        session.wait("대기 중인 승인 요청이 없습니다")
        session.click("대화")
        session.send("first request\r")
        session.wait("데모 스트리밍")
        session.send("second queued\r")
        session.wait("대기열 · 1개")
        session.send("third queued\r")
        session.wait("대기열 · 2개")
        queue_x, queue_y = session.screen.position("대기열 · 2개")
        assert session.screen.foregrounds[queue_y][queue_x] == (117, 200, 230), "Queue header is not visually distinct"
        assert "대기열에 추가" not in "".join(session.screen.cells[-1]), "Redundant queue button still visible"
        composer_y = session.screen.position("메시지 입력")[1]
        assert queue_y < composer_y, "Queue is not docked above the composer"
        session.wait("› second queued", timeout=10)
        session.wait("› third queued", timeout=10)
    finally:
        session.close()
        workdir.cleanup()
    print("PTY queue: add during streaming, FIFO auto-send after completion OK", flush=True)


def force_queue_test():
    workdir = tempfile.TemporaryDirectory(prefix="custom-tui-force-queue-")
    session = Session("--demo", cwd=workdir.name)
    try:
        session.wait("실제 실행 없음")
        session.click("승인")
        session.wait("이번 요청 허용")
        session.click("거절")
        session.wait("대기 중인 승인 요청이 없습니다")
        session.click("대화")
        session.send("running request\r")
        session.wait("데모 스트리밍")
        session.send("second queued\r")
        session.send("third queued\r")
        session.wait("대기열 · 2개")
        _, target_y = session.screen.position("third queued")
        button_x, _ = session.screen.position("즉시")
        session.send(f"\x1b[<0;{button_x+1};{target_y+1}M\x1b[<0;{button_x+1};{target_y+1}m")
        session.wait("› third queued", timeout=6)
        assert "› second queued" not in session.screen.text(), "Wrong queue entry ran first"
        session.wait("› second queued", timeout=12)
    finally:
        session.close()
        workdir.cleanup()
    print("PTY force queue: interrupt active response, promote selected message, retain remaining order OK", flush=True)


def clipboard_test():
    with tempfile.TemporaryDirectory(prefix="custom-tui-clipboard-") as directory:
        directory = Path(directory)
        clipboard = directory / "clipboard.txt"
        clipboard.write_text("단축키 붙여넣기", encoding="utf-8")
        for name, script in {
            "pbcopy": '#!/bin/sh\ncat > "$TEST_CLIPBOARD"\n',
            "xclip": '#!/bin/sh\nif [ "$1" = "-selection" ] && [ "$3" = "-o" ]; then cat "$TEST_CLIPBOARD"; else cat > "$TEST_CLIPBOARD"; fi\n',
            "pbpaste": '#!/bin/sh\ncat "$TEST_CLIPBOARD"\n',
        }.items():
            path = directory / name
            path.write_text(script)
            path.chmod(0o700)
        session = Session("--demo", cwd=directory, env={
            "PATH": f"{directory}:{os.environ['PATH']}",
            "TEST_CLIPBOARD": str(clipboard),
        })
        try:
            session.wait("대화는 넓게")
            # The copy icon uses the same row as the message it copies.
            _, row = session.screen.position("대화는 넓게")
            session.send(f"\x1b[<0;{session.screen.position('⧉')[0]+1};{row+1}M")
            session.send(f"\x1b[<0;{session.screen.position('⧉')[0]+1};{row+1}m")
            session.wait("복사 완료")
            assert clipboard.read_text().startswith("대화는 넓게"), "Copy icon copied the wrong message"
            session.send("x")
            session.wait("› x")  # Copy click must immediately return keyboard focus.
            session.send("\x7f")
            clipboard.write_text("단축키 붙여넣기", encoding="utf-8")
            session.click("파일 변경 1")
            session.send("\x16")  # Ctrl+V reads the text clipboard and focuses the composer.
            session.wait(f"[붙여넣은 내용 · {len('단축키 붙여넣기')}자]")
            session.wait("● 대화")
            session.click("파일 변경 1")
            session.send("\x1b[200~브래킷 붙여넣기\n여러 줄\x1b[201~")
            session.wait(f"[붙여넣은 내용 · {len('브래킷 붙여넣기' + chr(10) + '여러 줄')}자]")
            session.wait("● 대화")
            assert "브래킷 붙여넣기" not in session.screen.text(), "Raw paste leaked into the composer"
            session.click("승인")
            session.click("거절")
            session.click("대화")
            session.send("\r")
            session.wait("단축키 붙여넣기")
            session.wait("브래킷 붙여넣기")
            session.wait("여러 줄")
        finally:
            session.close()
    print("PTY clipboard: message copy, Ctrl+V and bracketed multiline paste from another tab OK", flush=True)


def live_test(model=None):
    owner = Session(*(["--model", model] if model else []))
    viewer = None
    resumed = None
    try:
        owner.wait("Loom")
        owner.wait(model or "준비됨",timeout=12)
        owner.send("Reply with exactly TUI_STREAM_OK. Do not use tools or modify files.\r")
        owner.wait("TUI_STREAM_OK", timeout=40)
        # Distinguish the actual assistant response from the user message.
        owner.wait("● TUI_STREAM_OK", timeout=40)
        thread_id = (ROOT / ".custom-tui/last-thread").read_text().strip()
        owner.wait("완료",timeout=12)
        owner.click("● TUI_STREAM_OK")
        owner.wait("⑂")
        owner.click("⑂")
        owner.wait("새 분기 시작",timeout=15)
        branch_id=(ROOT / ".custom-tui/last-thread").read_text().strip()
        assert branch_id!=thread_id,"Fork reused the original thread ID"
        viewer = Session("--agent", thread_id)
        viewer.wait("TUI_STREAM_OK", timeout=12)
        owner.close()
        owner = None
        viewer.wait("TUI_STREAM_OK")
        resumed = Session("--resume", branch_id)
        resumed.wait("TUI_STREAM_OK", timeout=12)
    finally:
        for session in [owner, viewer, resumed]:
            if session is not None:
                session.close()
    print("Live Codex: inference, second-client history, owner close, session resume, real turn fork with original preserved OK", flush=True)


def focus_recovery_test():
    with tempfile.TemporaryDirectory(prefix="custom-tui-focus-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.wait("실제 실행 없음")
            session.send("\t")  # Move keyboard focus to a UI control.
            session.send("type")  # Typing returns to the composer.
            session.wait("› type")
            session.click("도움말")
            session.wait("명령 안내")
            session.send("more")  # A compact detail pane also yields to typing.
            session.wait("● 대화")
            session.wait("› typemore")
        finally:
            session.close()
    print("PTY focus: typed characters recover composer from focused controls and compact details OK", flush=True)


def keyboard_only_commands_test():
    with tempfile.TemporaryDirectory(prefix="custom-tui-keyboard-") as directory:
        session = Session("--demo", cwd=directory, show_popup=True)
        try:
            session.wait("승인 필요")
            session.send("\x1b")  # Dismiss the approval popup without the mouse.
            session.send("/diff\r")
            session.wait("새 Rust 화면 엔진")
            session.send("\x1b")
            session.wait("● 대화")
            session.send("/tool 1\r")
            session.send("/copy 1\r")
            session.send("/approvals\r")
            session.wait("1. 파일 변경 승인")
            session.wait("2. 거절")
            session.send("/approve 1 2\r")
            session.send("/approvals\r")
            session.wait("대기 중인 승인 요청이 없습니다")
            session.send("/sessions\r")
            session.wait("최근 대화")
            session.send("/chat\r")
            session.wait("● 대화")
            session.resize(120, 40)
            session.wait("Loom")
            session.send("/settings panel\r")
            session.wait("● 대화")  # Hiding the panel must stay hidden.
            session.send("/settings panel\r")
            session.wait("● 설정")
            session.wait("시작 시 작업 패널")
        finally:
            session.close()
    print("PTY keyboard: commands navigate diff, tool, copy, approval, sessions, chat and panel OK", flush=True)


def local_exploration_test():
    with tempfile.TemporaryDirectory(prefix="custom-tui-explore-") as directory:
        Path(directory, "auth.rs").write_text("pub fn refresh_session() {}\n")
        session = Session("--demo", "--codex-bin", "/does/not/exist", cwd=directory)
        try:
            session.send("/explore refresh_session\r")
            session.wait("LOCAL · 공유 코드 탐색")
            session.wait("auth.rs:1")
            session.wait("API 호출 0")
            session.send("/explore refresh_session\r")
            session.wait("unique searches 1")
        finally:
            session.close()
        output = subprocess.check_output([
            str(BINARY), "--cwd", directory, "--codex-bin", "/does/not/exist", "--explore", "refresh_session"
        ], text=True)
        assert "auth.rs:1" in output and "repo://" in output
    print("PTY LOCAL: shared exploration, cached query, offline CLI without Codex OK", flush=True)


def question_form_test():
    with tempfile.TemporaryDirectory(prefix="loom-question-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.send("질문 데모\r")
            session.wait("어느 기능부터 구현할까요?")
            session.click("답변 제출")
            session.wait("모든 질문에 답해주세요")
            session.click("질문 UI")
            session.send("\t")
            session.wait("구현 시 고려할 사항을 입력해주세요.")
            session.send("\x1b[200~초안 보존\x1b[201~")
            session.wait("초안 보존")
            session.send("\x1b")
            session.wait("질문 1 · 열기")
            session.click("질문 1 · 열기")
            session.wait("초안 보존")
            session.resize(60, 20)
            session.wait("답변 제출")
            session.click("답변 제출")
            session.wait("데모 답변 수신 완료")
        finally:
            session.close()
    print("PTY questions: explicit options, custom paste, validation, hide/reopen, resize and submit OK", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--model")
    options = parser.parse_args()
    approval_popup_test()
    session_menu_test()
    demo_test()
    paging_test()
    queue_test()
    force_queue_test()
    clipboard_test()
    focus_recovery_test()
    keyboard_only_commands_test()
    local_exploration_test()
    question_form_test()
    if options.live:
        live_test(options.model)
