"""Exercise the actual binary through a PTY. Default tests never call a model.

Run with --live to additionally send one short, tool-free inference request,
read it from a second client, and resume it after closing the first window.
"""
import argparse
import codecs
import json
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
BINARY = Path(os.environ.get("CUSTOM_TUI_BINARY", ROOT / "loom-tui/target/debug/loom"))


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
        self.attributes = [[set() for _ in range(width)] for _ in range(height)]
        self.active_attributes = set()
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
                    position = 0
                    while position < len(parts):
                        part = parts[position]
                        # RGB/indexed color components are data, not SGR attributes.
                        if part in (38, 48) and position + 1 < len(parts):
                            mode = parts[position + 1]
                            if mode == 2 and position + 4 < len(parts):
                                color = tuple(parts[position + 2:position + 5])
                                if part == 38:
                                    self.fg = color
                                else:
                                    self.bg = color
                                position += 5
                                continue
                            if mode == 5 and position + 2 < len(parts):
                                position += 3
                                continue
                        if part == 0:
                            self.active_attributes.clear()
                            self.fg = self.bg = None
                        elif part == 1:
                            self.active_attributes.add("bold")
                        elif part == 3:
                            self.active_attributes.add("italic")
                        elif part == 4:
                            self.active_attributes.add("underline")
                        elif part == 9:
                            self.active_attributes.add("strike")
                        elif part == 22:
                            self.active_attributes.discard("bold")
                        elif part == 23:
                            self.active_attributes.discard("italic")
                        elif part == 24:
                            self.active_attributes.discard("underline")
                        elif part == 29:
                            self.active_attributes.discard("strike")
                        elif part == 39:
                            self.fg = None
                        elif part == 49:
                            self.bg = None
                        position += 1
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
                    self.attributes[self.y][self.x] = self.active_attributes.copy()
                    if width == 2 and self.x + 1 < self.width:
                        self.cells[self.y][self.x + 1] = ""
                    self.x += width
                elif width == 0 and self.y < self.height and 0 < self.x <= self.width:
                    self.cells[self.y][self.x - 1] += ch
            i += 1
        self.buffer = self.buffer[i:]

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.cells)

    def position(self, text, start_row=0):
        for y, row in enumerate(self.cells[start_row:], start_row):
            for x in range(self.width):
                if "".join(row[x:]).startswith(text):
                    return x, y
        raise AssertionError(f"Not visible: {text}\n{self.text()}")


class Session:
    def __init__(self, *args, cwd=None, env=None, show_popup=False, explicit_cwd=True):
        self.master, self.slave = pty.openpty()
        self.screen = Screen()
        self.raw = bytearray()
        self.resize(80, 24)
        self.before = termios.tcgetattr(self.slave)
        directory_args = ["--cwd", str(cwd or ROOT)] if explicit_cwd else []
        self.process = subprocess.Popen([str(BINARY), *directory_args, *args], stdin=self.slave, stdout=self.slave, stderr=self.slave, start_new_session=True, env={**{k:v for k,v in os.environ.items() if k != "NO_COLOR"}, "TERM": "xterm-256color", **(env or {})})
        if "--demo" in args and not show_popup:
            self.wait("승인 필요")
            self.send("\x1b")
            self.wait("메시지 입력…")

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
                self.raw.extend(data)
                self.screen.feed(data)

    def send(self, text):
        os.write(self.master, text.encode())
        self.pump()

    def command(self, text):
        # Exact slash commands keep the suggestion menu open. The first Enter
        # accepts the suggestion into the composer; the second submits it.
        self.send(text + "\r")
        self.send("\r")

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
        self.wait(text)
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


def screen_rgb_sgr_test():
    screen = Screen()
    screen.feed(b"\x1b[4m\x1b[38;2;49;0;24m\x1b[48;2;18;24;30mX")
    assert screen.attributes[0][0] == {"underline"}
    assert screen.foregrounds[0][0] == (49, 0, 24)
    assert screen.backgrounds[0][0] == (18, 24, 30)
    screen.feed(b"\x1b[24;39;49mY")
    assert not screen.attributes[0][1]
    assert screen.foregrounds[0][1] is None and screen.backgrounds[0][1] is None


def settings_panel_stability_test():
    import json
    with tempfile.TemporaryDirectory(prefix="loom-settings-panel-") as cwd:
        state = Path(cwd) / ".custom-tui"
        state.mkdir()
        (state / "settings.json").write_text(json.dumps({"panel_open": True, "activity_panel": False}))
        session = Session("--demo", cwd=cwd)
        try:
            session.resize(140, 60)
            session.wait("메시지 입력…")
            assert "┬" not in "".join(session.screen.cells[1]), "Saved hidden activity panel appeared on startup"
            session.command("/settings")
            session.wait("인터페이스 밀도")
            assert "┬" in "".join(session.screen.cells[1]), "Settings moved out of its side pane"
            x, y = session.screen.position("대화 옆 현황")
            assert x > 80, session.screen.text()
            for _ in range(3):
                for choice in ("○ 표시", "○ 숨김"):
                    # The first Show/Hide choice is the activity panel row.
                    assert session.screen.position(choice)[1] == y
                    session.click(choice)
                    session.wait("인터페이스 밀도")
                    assert "┬" in "".join(session.screen.cells[1])
                    assert session.screen.position("대화 옆 현황")[0] == x
            session.command("/chat")
            session.wait("● 대화")
            assert "┬" not in "".join(session.screen.cells[1])
            session.send("still responsive")
            session.wait("› still responsive")
        finally:
            session.close()
    print("PTY settings panel: opening with hidden activity, repeated toggles and composer remain stable OK", flush=True)


def checkpoint_responsive_test():
    import shlex
    with tempfile.TemporaryDirectory(prefix="loom-checkpoint-") as directory:
        cwd = Path(directory) / "repo"
        cwd.mkdir()
        marker = Path(directory) / "checkpoint-pid"
        subprocess.run(["git", "init", "-q"], cwd=cwd, check=True)
        # Revisions now bypass clean filters to preserve raw bytes. Delay the
        # actual raw hashing command, retaining the responsiveness/cancel check.
        import shutil
        real_git = shutil.which("git")
        wrappers = Path(directory) / "bin"
        wrappers.mkdir()
        wrapper = wrappers / "git"
        wrapper.write_text(f'#!/bin/sh\ncase " $* " in *" hash-object "*) echo $$ > {shlex.quote(str(marker))}; sleep 30;; esac\nexec {shlex.quote(real_git)} "$@"\n')
        wrapper.chmod(0o755)
        (cwd / "source.txt").write_text("source\n")
        session = Session("--demo", cwd=cwd, env={"PATH": str(wrappers) + os.pathsep + os.environ["PATH"]})
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.send("checkpoint responsiveness\r")
            deadline = time.monotonic() + 8
            while not marker.exists() and time.monotonic() < deadline:
                session.pump()
            assert marker.exists(), "Slow checkpoint did not start"
            session.command("/settings")
            session.wait("인터페이스 밀도", timeout=2)
            session.send("\x11")
            assert session.process.wait(timeout=2) == 0, "Ctrl+Q did not exit during Git checkpoint"
            filter_pid = int(marker.read_text())
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                try:
                    os.kill(filter_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(.02)
            else:
                raise AssertionError("Checkpoint Git process survived shutdown")
        finally:
            session.close()
    print("PTY checkpoint: settings and Ctrl+Q respond during slow Git; shutdown cancels raw hashing OK", flush=True)


def grouped_settings_test():
    import json
    with tempfile.TemporaryDirectory(prefix="loom-settings-") as cwd:
        state = Path(cwd) / "launcher"
        state.mkdir()
        env = {"LOOM_SETTINGS_STATE_DIR": str(state)}
        session = Session("--demo", cwd=cwd, env=env)
        try:
            session.resize(80, 60)
            session.command("/settings")
            session.wait("설정 초기화")
            session.click("○ 조밀")
            session.wait("● 조밀")
            session.click("○ 항상 펼침")
            session.wait("● 항상 펼침")
            session.click("○ 즉시 전송")
            session.wait("● 즉시 전송")
            session.click("○ 비움")
            session.wait("● 비움")
            session.send("/settings show_tokens false\r")
            assert "tokens" not in session.screen.text()
            session.send("/settings timestamps true\r")
            session.send("/settings startup_screen last\r")
            session.send("/settings reopen_workspace true\r")
            saved = json.loads((Path(cwd) / ".custom-tui/settings.json").read_text())
            assert saved["density"] == "compact" and saved["tool_details"] == "expanded"
            assert saved["enter_mode"] == "immediate" and saved["input_history"] == "clear"
            session.click("악센트 색상")
            session.wait("← 설정")
            session.click("형광초록")
            session.wait("● 형광초록")
            session.send("\x1b")
            session.wait("설정 초기화")
            session.click("로그 및 진단")
            session.wait("최근 알림과 오류")
            session.click("진단 파일 저장")
            session.wait("진단 파일 저장:")
            diagnostic = json.loads((Path(cwd) / ".custom-tui/diagnostics.json").read_text())
            assert diagnostic["preferences"]["density"] == "compact"
            session.send("\x1b")
            session.resize(40, 24)
            session.wait("인터페이스 밀도")
            for _ in range(65):
                session.send("\x1b[B")
                if "설정 초기화" in session.screen.text():
                    break
            session.wait("설정 초기화")
        finally:
            session.close()
        # The real launcher resolves its last-workspace pointer only without --cwd.
        (state / "last-workspace.json").write_text(json.dumps(cwd))
        session = Session("--demo", env=env, explicit_cwd=False)
        try:
            session.wait("인터페이스 밀도")
            session.wait("● 조밀")
            session.resize(80, 60)
            session.wait("설정 초기화")
            session.click("설정 초기화")
            session.wait("UI 설정을 초기화")
            x, y = session.screen.position("   초기화")
            session.send(f"\x1b[<0;{x+4};{y+1}M")
            session.send(f"\x1b[<0;{x+4};{y+1}m")
            session.wait("● 기본")
            saved = json.loads((Path(cwd) / ".custom-tui/settings.json").read_text())
            assert saved["density"] == "normal" and saved["accent"] == "cyan"
            assert "대화·파일·권한" not in session.screen.text()
        finally:
            session.close()
    print("PTY settings: grouped choices, mouse, narrow keyboard scrolling, diagnostics, startup restore and reset OK", flush=True)


def accent_settings_test():
    import json
    with tempfile.TemporaryDirectory(prefix="loom-accent-") as cwd:
        session = Session("--demo", cwd=cwd)
        try:
            for value, label, accent, warning in [
                ("white", "흰색", (240, 244, 248), (242, 222, 170)),
                ("hacker", "형광초록", (57, 255, 20), (220, 255, 0)),
            ]:
                session.send(f"/settings accent {value}\r")
                session.wait(f"● {label}")
                x, y = session.screen.position("● 설정")
                assert session.screen.foregrounds[y][x] == accent, "Active tab did not repaint"
                x, y = session.screen.position(f"● {label}")
                assert session.screen.foregrounds[y][x] == accent, "Current palette did not repaint"
                assert any(warning in row for row in session.screen.foregrounds), "Pending approval color did not change"
                saved = json.loads((Path(cwd) / ".custom-tui/settings.json").read_text())
                assert saved["accent"] == value, "Palette was not saved"
            session.click("흰색")
            session.wait("● 흰색")
            saved = json.loads((Path(cwd) / ".custom-tui/settings.json").read_text())
            assert saved["accent"] == "white", "Mouse palette selection was not saved"
            session.resize(140, 36)
            session.wait("● 흰색")
        finally:
            session.close()
        session = Session("--demo", cwd=cwd)
        try:
            session.command("/settings")
            session.wait("● 흰색")
            x, y = session.screen.position("● 설정")
            assert session.screen.foregrounds[y][x] == (240, 244, 248), "Saved palette was not restored"
        finally:
            session.close()
    print("PTY accent: keyboard, mouse, status colors, resize and restart persistence OK", flush=True)


def demo_test():
    workdir=tempfile.TemporaryDirectory(prefix="custom-tui-demo-")
    session = Session("--demo",cwd=workdir.name)
    try:
        session.wait("세션")
        session.send("\x1b[200~입력 초안\n두 번째 줄\x1b[201~")
        session.wait(f"[붙여넣은 내용 · {len('입력 초안' + chr(10) + '두 번째 줄')}자]")
        session.click("작업")
        session.wait("하위 에이전트")
        session.click("대화")
        session.wait("[붙여넣은 내용")
        session.click("파일 변경 1")
        session.wait("새 Rust 화면 엔진")
        dx, dy = session.screen.position("새 Rust 화면 엔진")
        assert session.screen.backgrounds[dy][dx] == (15, 48, 24), "Added diff line has no background"
        rx, ry = session.screen.position("기존 TUI 수정")
        assert session.screen.backgrounds[ry][rx] == (57, 22, 26), "Deleted diff line has no background"
        session.click("작업")
        session.click("승인")
        session.wait("파일 변경 승인")
        session.wait("이번 요청 허용")
        session.click("거절")
        session.wait("대기 중인 승인 요청이 없습니다")
        session.click("대화")
        session.wait("[붙여넣은 내용")
        session.resize(120, 40)
        session.wait("대화는 넓게")
        assert "⑂" not in session.screen.text(), "Branch controls must not consume conversation width"
        session.wait("[붙여넣은 내용")
        # Actual mouse motion highlights a button while the selected tab stays marked.
        x, y = session.screen.position("▤")
        session.send(f"\x1b[<35;{x+1};{y+1}M")
        deadline = time.monotonic() + 3
        while session.screen.backgrounds[y][x] != (55, 58, 62) and time.monotonic() < deadline:
            session.pump()
        assert session.screen.backgrounds[y][x] == (55, 58, 62), "No mouse hover feedback"
        ax, ay = session.screen.position("● 대화")
        assert (
            session.screen.foregrounds[ay][ax] == (117, 200, 230)
            or session.screen.backgrounds[ay][ax] == (66, 69, 73)
        ) and session.screen.backgrounds[ay][ax] != (55, 58, 62), "Active tab lost during hover"
        session.click("▤")
        session.wait("에이전트가 보고한 파일 변경")
        session.wait("새 Rust 화면 엔진")
        chat_x, _ = session.screen.position("대화는 넓게")
        diff_x, _ = session.screen.position("에이전트가 보고한 파일 변경")
        assert chat_x < diff_x, "Wide diff should keep chat on the left and expand detail on the right"
        session.wait("[붙여넣은 내용")
        # Entering text in the composer leaves the selected detail partition open.
        session.click("붙여넣은 내용 · 12자")
        session.send("x")
        session.wait("에이전트가 보고한 파일 변경")
        session.click("대화")
        session.wait("let message")
        cx, cy = session.screen.position("let message")
        assert session.screen.backgrounds[cy][cx] == (49, 56, 65), "Code block has no background"
        session.send("\x01")
        session.send("\x1b[3~")  # Delete the selected draft in one step.
        session.command("/skills")
        session.wait("설치된 스킬 1개")
        session.click("$demo-review")
        session.wait("$demo-review ×")
        session.send("\x1b[200~검토 초안\x1b[201~")
        session.wait(f"[붙여넣은 내용 · {len('검토 초안')}자]")
        session.click("환경")
        session.wait("● 모델")
        session.wait("다음 메시지에 사용할 모델")
        session.click("○ fixture-fast")
        session.wait("fixture-fast")
        session.wait("[붙여넣은 내용")
        session.click("설정")
        session.wait("대화 옆 현황")
        session.resize(80,24)
        session.wait("● 설정")
        session.click("대화")
        session.wait("[붙여넣은 내용")
        session.click("설정")
        session.wait("대화 옆 현황")
        session.resize(80, 40)
        session.click("○ English")
        session.wait("● Settings")
        session.wait("Overview panel")
        session.click("○ 한국어")
        session.wait("● 설정")
        session.click("대화")
        session.click("대화는 넓게")
        assert "⑂" not in session.screen.text()
        # Body text remains quiet on hover/click; dragging visibly selects it.
        mx, my = session.screen.position("대화는 넓게")
        assert session.screen.backgrounds[my][mx] != (66, 69, 73)
        session.send(f"\x1b[<35;{mx+1};{my+1}M")
        assert session.screen.backgrounds[my][mx] != (55, 58, 62)
        session.send(f"\x1b[<0;{mx+1};{my+1}M")
        session.send(f"\x1b[<32;{mx+10};{my+1}M")
        assert session.screen.backgrounds[my][mx] == (66, 69, 73)
        session.send(f"\x1b[<0;{mx+10};{my+1}m")
        session.click("대화는 넓게")
        session.send("x")
        session.wait("[붙여넣은 내용 · 5자]x")  # Text selection preserves the draft and keyboard focus.
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
            session.send("\x1b")
            session.wait("메시지 입력…")
            assert "승인 필요" not in session.screen.text(), "Dismissed approval card remains visible"
            session.click("작업 !1")
            session.wait("승인 !1")
            session.click("승인 !1")
            session.wait("이번 요청 허용")
            session.wait("거절")
        finally:
            session.close()
    print("PTY approval popup: dismiss preserves pending approval and Work > Approval reopens choices OK", flush=True)


def approval_keyboard_test():
    with tempfile.TemporaryDirectory(prefix="loom-keyboard-approval-") as directory:
        session = Session("--demo", cwd=directory, show_popup=True)
        try:
            session.wait("승인 필요")
            decision_y = session.screen.position("승인 필요")[1]
            assert decision_y >= session.screen.height // 2, "Approval is not docked below the transcript"
            session.send("\x1b[B\r")  # Down, Enter: reject; no mouse events.
            session.wait("데모 승인 선택 완료")
            session.wait("메시지 입력…")
            assert "승인 필요" not in session.screen.text()
        finally:
            session.close()
    print("PTY approval composer: Down/Enter responds without mouse OK", flush=True)


def session_menu_test():
    with tempfile.TemporaryDirectory(prefix="custom-tui-sessions-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.wait("세션")
            session.click("세션")
            session.wait("현재 대화")
            session.wait("최근 대화")
            session.wait("+ 새 대화")
            assert "demo-saved" not in session.screen.text(), "Raw session IDs leaked into the menu"
            # A pending approval explains why selection is temporarily blocked.
            assert "승인 요청을 처리한 후" in session.screen.text()
            session.click("작업 !1")
            session.wait("승인 !1")
            session.click("승인 !1")
            session.click("거절")
            session.click("세션")
            # The whole session row is a click target, not just a small text button.
            sx, sy = session.screen.position("예시 · 이전 화면 검토")
            session.send(f"\x1b[<0;{session.screen.width-3};{sy+1}M\x1b[<0;{session.screen.width-3};{sy+1}m")
            session.wait("새 데모 대화")
            session.click("세션")
            session.wait("● 예시 · 이전 화면 검토")
            session.click("TUI 화면 검토")
            session.wait("새 데모 대화")
            session.click("세션")
            # The session selector remains available through subsequent switches.
            session.click("예시 · 이전 화면 검토")
            session.wait("새 데모 대화")
            session.click("세션")
            session.wait("● 예시 · 이전 화면 검토")
            # At desktop width the session pane is on the right. It must also
            # support clicking the full width of each row.
            session.resize(120, 40)
            session.wait("TUI 화면 검토")
            sx, sy = session.screen.position("TUI 화면 검토")
            session.send(f"\x1b[<0;{session.screen.width-3};{sy+1}M\x1b[<0;{session.screen.width-3};{sy+1}m")
            session.wait("새 데모 대화")
            session.click("세션")
            session.wait("예시 · 이전 화면 검토")
            session.click("+ 새 대화")
            session.wait("새 데모 대화")
            session.click("세션")
            session.wait("● 새 대화")
            session.command("/sessions")
            session.wait("1. ○ TUI 화면 검토")
            session.wait("2. ○ 예시 · 이전 화면 검토")
            session.send("/sessions 2\r")
            session.wait("새 데모 대화")
            session.click("세션")
            session.wait("● 예시 · 이전 화면 검토")
        finally:
            session.close()
    print("PTY sessions: current, recent, switch, new and clean labels OK", flush=True)


def paging_test():
    workdir = tempfile.TemporaryDirectory(prefix="custom-tui-paging-")
    session = Session("--demo", cwd=workdir.name)
    try:
        session.wait("세션")
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
        session.wait("세션")
        session.click("작업 !1")
        session.wait("승인 !1")
        session.click("승인 !1")
        session.wait("파일 변경 승인")
        session.wait("이번 요청 허용")
        session.click("거절")
        session.wait("대기 중인 승인 요청이 없습니다")
        session.click("대화")
        session.send("first request\r")
        session.wait("코드를 짜는 중")
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
        session.wait("세션")
        session.click("작업 !1")
        session.wait("승인 !1")
        session.click("승인 !1")
        session.wait("이번 요청 허용")
        session.click("거절")
        session.wait("대기 중인 승인 요청이 없습니다")
        session.click("대화")
        session.send("running request\r")
        session.wait("코드를 짜는 중")
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


def force_queue_keyboard_test():
    with tempfile.TemporaryDirectory(prefix="loom-keyboard-queue-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.command("/new")
            session.wait("새 데모 대화")
            session.send("running request\r")
            session.wait("코드를 짜는 중")
            session.send("keyboard priority\r")
            session.wait("대기열 · 1개")
            session.send("\x0e")  # Ctrl+N
            session.wait("› keyboard priority", timeout=6)
        finally:
            session.close()
    print("PTY Ctrl+N: first queued message promoted without mouse OK", flush=True)


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
            assert "⧉" not in session.screen.text()
            x, row = session.screen.position("대화는 넓게")
            session.send(f"\x1b[<0;{x+1};{row+1}M")
            session.send(f"\x1b[<32;{x+12};{row+1}M")
            assert session.screen.backgrounds[row][x] == (66, 69, 73)
            session.send(f"\x1b[<0;{x+12};{row+1}m")
            session.wait("복사 완료")
            assert clipboard.read_text() == "대화는 넓게", "Drag copied UI controls or wrong text"
            session.click("TUI 구조 읽기 (예시)")
            session.wait("위치: Loom")
            tx, ty = session.screen.position("TUI 구조 읽기 (예시)")
            ox, oy = session.screen.position("위치: Loom")
            session.send(f"\x1b[<0;{tx+1};{ty+1}M")
            session.send(f"\x1b[<32;{ox+12};{oy+1}M")
            session.send(f"\x1b[<0;{ox+12};{oy+1}m")
            session.wait("복사 완료")
            copied_tool = clipboard.read_text()
            assert "TUI 구조 읽기 (예시)" in copied_tool, "Tool header was not copied"
            assert "위치: Loom" in copied_tool, f"Tool output was not copied: {copied_tool!r}"
            session.send("x")
            session.wait("› x")  # Drag copying leaves typing in the composer.
            session.send("\x7f")
            session.send("가나다")
            ix, iy = session.screen.position("가나다")
            session.send(f"\x1b[<0;{ix+3};{iy+1}M\x1b[<0;{ix+3};{iy+1}m")
            session.send("!")
            session.wait("가!나다")
            ix, iy = session.screen.position("가!나다")
            session.send(f"\x1b[<0;{ix+1};{iy+1}M\x1b[<32;{ix+4};{iy+1}M\x1b[<0;{ix+4};{iy+1}m")
            session.send("替")
            session.wait("替나다")
            session.send("\x01")  # Ctrl+A: replace the entire draft.
            session.send("새입력")
            session.wait("› 새입력")
            session.send("\x01\x1b[3~")
            clipboard.write_text("단축키 붙여넣기", encoding="utf-8")
            session.click("파일 변경 1")
            session.send("\x16")  # Ctrl+V reads the text clipboard and focuses the composer.
            session.wait(f"[붙여넣은 내용 · {len('단축키 붙여넣기')}자]")
            session.wait("● 대화")
            session.click("▤")
            session.send("\x1b[200~브래킷 붙여넣기\n여러 줄\x1b[201~")
            session.wait(f"[붙여넣은 내용 · {len('브래킷 붙여넣기' + chr(10) + '여러 줄')}자]")
            session.wait("● 대화")
            assert "브래킷 붙여넣기" not in session.screen.text(), "Raw paste leaked into the composer"
            session.click("작업 !1")
            session.wait("승인 !1")
            session.click("승인 !1")
            session.click("거절")
            session.click("대화")
            session.send("\r")
            session.wait("붙여넣은 내용을 전송할까요?")
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
        owner.command("/branch 1")
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
            session.wait("세션")
            session.send("\t")  # Move keyboard focus to a UI control.
            session.send("type")  # Typing returns to the composer.
            session.wait("› type")
            session.click("설정")
            session.wait("대화 옆 현황")
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
            session.command("/diff")
            session.wait("새 Rust 화면 엔진")
            session.send("\x1b")  # Leave the composer, then return from detail.
            session.send("\x1b")
            session.wait("● 대화")
            session.send("/tool 1\r")
            session.send("/copy 1\r")
            session.command("/approvals")
            session.wait("1. 파일 변경 승인")
            session.wait("2. 거절")
            session.send("/approve 1 2\r")
            session.command("/approvals")
            session.wait("대기 중인 승인 요청이 없습니다")
            session.command("/sessions")
            session.wait("최근 대화")
            session.command("/chat")
            session.wait("● 대화")
            session.resize(120, 40)
            session.wait("● 대화")
            session.send("/settings panel\r")
            session.wait("작업 패널")  # Opens Settings at the startup panel preference.
            session.send("/settings panel\r")
            session.wait("● 대화")
            assert "● 설정" not in session.screen.text(), "Closing the panel left Settings visible"
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
            assert session.screen.position("답변 제출")[1] == session.screen.height - 3
            assert "메시지 입력…" not in session.screen.text()
            session.click("답변 제출")
            session.wait("모든 질문에 답해주세요")
            session.click("질문 UI")
            # Tab now traverses the visible question controls, including Cancel.
            session.click("다음")
            session.wait("구현 시 고려할 사항을 입력해주세요.")
            session.send("\x1b[200~초안 보존\x1b[201~")
            session.wait("초안 보존")
            session.send("\x1b")
            session.wait("질문 1 · 열기")
            session.click("질문 1 · 열기")
            session.wait("초안 보존")
            session.resize(60, 20)
            session.wait("답변 제출")
            assert session.screen.position("답변 제출")[1] == session.screen.height - 3
            session.resize(40, 12)
            session.wait("초안 보존")
            session.wait("제출")
            assert session.screen.position("제출")[1] == session.screen.height - 3
            session.click("제출")
            session.wait("데모 답변 수신 완료")
        finally:
            session.close()
    print("PTY questions: explicit options, custom paste, validation, hide/reopen, resize and submit OK", flush=True)


def escape_interrupt_test():
    with tempfile.TemporaryDirectory(prefix="loom-escape-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.send("활동 데모\r")
            session.wait("활동 그래프 데모")
            session.send("보존할 초안")
            session.wait("보존할 초안")
            session.send("\x1b")
            session.wait("중단됨")
            assert "보존할 초안" in session.screen.text()
            session.send("\r")  # The preserved draft can be sent after interruption.
            session.wait("예시 응답이야")
        finally:
            session.close()
    with tempfile.TemporaryDirectory(prefix="loom-escape-menu-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.send("활동 데모\r")
            session.wait("활동 그래프 데모")
            session.send("/")  # Opens slash-command suggestions while the turn runs.
            session.wait("/help")
            session.send("\x1b")
            session.wait("중단됨")
            assert "/" in session.screen.text(), "Escape discarded the menu draft"
        finally:
            session.close()
    print("PTY Escape: active turn and slash-menu stop immediately, drafts survive OK", flush=True)


def activity_graph_test():
    with tempfile.TemporaryDirectory(prefix="loom-activity-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.send("활동 데모\r")
            session.wait("활동 그래프 데모")
            session.resize(140, 38)
            session.command("/agents")
            session.wait("전체 5 · 진행 2 · 완료 1")
            session.wait("MEDIUM")
            session.wait("원인 분석 결과가")
            session.send("/task 4\r")
            session.wait("작업 상세")
            session.wait("수정안 작성")
            session.wait("demo-analysis · 원인 분석")
            assert "Cost/cache: 0" not in session.screen.text(), "Missing usage was presented as measured zero"
            session.send("\x03")
            session.wait("사용자 중단 (fixture)")
        finally:
            session.close()
    print("PTY activity: worker labels, critical path, waiting reason, task detail and cancellation OK", flush=True)


def conversation_dividers_test():
    with tempfile.TemporaryDirectory(prefix="loom-dividers-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.command("/chat")
            session.resize(80, 40)
            session.send("divider first\r")
            session.wait("코드를 짜는 중")
            session.wait("✓ 완료", timeout=10)
            session.send("divider second\r")
            session.wait("› divider second")
            session.wait("✓ 완료", timeout=10)
            def dividers():
                return [(y, row) for y, row in enumerate(session.screen.cells) if "".join(row).strip() == "─" * 76]
            bars = dividers()
            assert len(bars) == 6, session.screen.text()
            for index, (y, _) in enumerate(bars):
                expected = (18, 24, 30) if (index // 2) % 2 == 0 else (20, 26, 32)
                assert session.screen.backgrounds[y][0] == expected
                assert session.screen.backgrounds[y][-1] == expected
            assert session.screen.foregrounds[bars[0][0]][2] != session.screen.foregrounds[bars[1][0]][2]
            assert "── 사용자" not in session.screen.text() and "── Loom" not in session.screen.text()
            session.resize(40, 40)
            session.wait("› divider second")
            x, y = session.screen.position("› divider second")
            assert session.screen.backgrounds[y][x] == (18, 24, 30)
            assert "── 사용자" not in session.screen.text() and "── Loom" not in session.screen.text()
        finally:
            session.close()
    print("PTY transcript: colored unlabeled turn dividers, streaming and narrow resize OK", flush=True)


def rewind_test():
    with tempfile.TemporaryDirectory(prefix="loom-rewind-") as directory:
        import subprocess
        subprocess.run(["git", "init", "-q", directory], check=True)
        code = Path(directory) / "code.txt"
        code.write_text("first version")
        session = Session("--demo", cwd=directory)
        def wait_checkpoint(turn, expected):
            # Turn completion queues an asynchronous capture. Wait for durable
            # checkpoint bytes before mutating the fixture, not a stale UI label.
            end = time.monotonic() + 15
            while time.monotonic() < end:
                session.pump()
                try:
                    points = json.loads((Path(directory) / ".custom-tui/checkpoints.json").read_text())
                    point = next((p for p in points.get("demo-main", []) if p["turn"] == turn), None)
                    if point:
                        saved = subprocess.check_output(["git", "show", f"{point['tree']}:code.txt"], cwd=directory).decode()
                        assert saved == expected, (turn, saved, expected)
                        return
                except (FileNotFoundError, json.JSONDecodeError):
                    pass
            raise AssertionError(f"Checkpoint {turn} was not persisted")
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.send("first checkpoint\r")
            session.wait("✓ 완료", timeout=35)
            wait_checkpoint("demo-turn-1", "first version")
            code.write_text("second version")
            session.send("second checkpoint\r")
            session.wait("✓ 완료", timeout=35)
            wait_checkpoint("demo-turn-2", "second version")
            session.send("\x1b")
            session.send("\x1b")
            session.wait("복원 시점 선택")
            # Work's four child actions precede the two restore points.
            for _ in range(6):
                session.send("\x1b[B")
            session.send("\r")
            session.wait("대화만 되돌리기")
            session.wait("복원 시점: demo-turn-1")
            assert code.read_text() == "second version", "Selecting a point must not restore yet"
            session.wait("+1 −1")
            session.click("대화와 코드 되돌리기")
            session.wait("대화와 코드 복원 완료", timeout=15)
            assert code.read_text() == "first version"
            assert "second checkpoint" not in session.screen.text(), session.screen.text()
            session.click("세션")
            session.wait("● 분기 ·")
        finally:
            session.close()
    print("PTY rewind: double Escape, selection, confirmation, code restore and new session OK", flush=True)


def input_history_test():
    with tempfile.TemporaryDirectory(prefix="loom-input-history-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.send("첫 기록 확인\r")
            session.wait("✓ 완료", timeout=35)
            session.send("둘째 기록 확인\r")
            session.wait("둘째 기록 확인", timeout=35)

            def composer():
                return "".join(session.screen.cells[session.screen.height - 3])

            session.send("\x1b[A")
            assert "둘째 기록 확인" in composer(), composer()
            session.send("\x1b[A")
            assert "첫 기록 확인" in composer(), composer()
            session.send("\x1b[B")
            assert "둘째 기록 확인" in composer(), composer()
            session.send("\x1b[B")
            assert "첫 기록 확인" not in composer() and "둘째 기록 확인" not in composer(), composer()
        finally:
            session.close()
    print("PTY input history: empty composer Up/Down recalls recent sent messages", flush=True)


def markdown_test():
    with tempfile.TemporaryDirectory(prefix="loom-markdown-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            session.command("/new")
            session.wait("새 데모 대화")
            session.resize(100, 50)
            # A resize replaces the test's screen buffer. Wait for the new
            # frame before typing so an in-flight redraw cannot eat input.
            session.wait("메시지 입력…")
            session.send("마크다운 데모\r")
            session.wait("✓ 완료", timeout=35)
            session.wait("x² + (a₁)/(b)")
            session.wait("Core → T0")
            session.wait("Core → T1")
            session.wait("구현 결과")
            session.wait("중요한 변경")
            session.wait("겹친 스타일")
            session.wait("밑줄 확인")
            session.wait("취소선")
            session.wait("refresh_session()")
            session.wait("E = mc²")
            session.wait("☐ 회귀 검사")
            session.wait("│ 원문은 복사할 때 유지됩니다.")
            code_x, code_y = session.screen.position("refresh_session()")
            quote_x, quote_y = session.screen.position("│ 원문은 복사할 때 유지됩니다.")
            assert session.screen.backgrounds[code_y][code_x] == (49, 56, 65)
            assert session.screen.backgrounds[quote_y][quote_x] == (31, 40, 50)
            assert "**중요한 변경**" not in session.screen.text()
            assert "## 구현 결과" not in session.screen.text()
            assert "| --- |" not in session.screen.text()
            assert "☑ 한글 표시" in session.screen.text()
            for label, attribute in [
                ("중요한 변경", "bold"),
                ("설명", "italic"),
                ("겹친 스타일", "bold"),
                ("취소선", "strike"),
                ("밑줄 확인", "underline"),
            ]:
                x, y = session.screen.position(label)
                assert attribute in session.screen.attributes[y][x], f"{label} has no {attribute}"
            x, y = session.screen.position("겹친 스타일")
            assert "italic" in session.screen.attributes[y][x]
            assert "\\(" not in session.screen.text() and "\\[" not in session.screen.text()
            # Removing the branch gutter lets this table fit at the supported
            # minimum width; preserve all columns instead of forcing stacking.
            session.resize(40, 50)
            session.wait("Markdown")
            session.wait("정상")
            assert session.screen.position("Markdown")[1] == session.screen.position("정상")[1]
            session.wait("let 값 = 42;")
            code_x, code_y = session.screen.position("let 값 = 42;")
            assert session.screen.backgrounds[code_y][code_x] == (49, 56, 65)
        finally:
            session.close()
    print("PTY Markdown: streamed headings, emphasis, inline code, task list, quote, table, code and full-width narrow layout OK", flush=True)


def image_attachment_test():
    import base64
    with tempfile.TemporaryDirectory(prefix="loom-image-input-") as directory:
        path = Path(directory) / "screen shot.png"
        path.write_bytes(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAaX2RUAAAAAASUVORK5CYII="))
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 2\r")
            session.wait("데모 승인 선택 완료")
            session.send(f"\x1b[200~'{path}'\x1b[201~")
            session.wait("이미지 · screen shot.png ×")
            assert "붙여넣은 내용" not in session.screen.text()
            session.click("이미지 · screen shot.png ×")
            assert "이미지 · screen shot.png ×" not in session.screen.text()
            # Native terminal drop: unbracketed, shell-escaped path beside a draft.
            session.send("설명해줘 ")
            escaped = str(path).replace(" ", "\\ ")
            session.send(escaped + " ")
            session.wait("이미지 · screen shot.png ×")
            session.wait("› 설명해줘")
            session.send("\x01\x1b[3~")
            session.wait("메시지 입력…")
            session.resize(40, 12)
            session.wait("이미지")
            session.resize(80, 24)
            session.send("\r")
            session.wait("[이미지 · screen shot.png]")
        finally:
            session.close()
    print("PTY images: bracketed and native file drop, draft preservation, remove, resize and image-only send OK", flush=True)


def model_effort_selection_test():
    with tempfile.TemporaryDirectory(prefix="loom-model-effort-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 2\r")
            session.wait("데모 승인 선택 완료")
            session.resize(120, 40)
            session.command("/model")
            session.wait("다음 메시지에 사용할 모델")
            session.click("○ fixture-fast")
            session.wait("선택한 모델 · fixture-fast")
            session.wait("Reasoning effort")
            session.click("○ effort high")
            session.wait("● effort high")
            session.wait("선택한 모델 · fixture-fast")
            session.command("/chat")
            session.wait("● 대화")
        finally:
            session.close()
    print("PTY models: selecting a model keeps effort controls visible and accepts effort immediately", flush=True)


def effort_and_quota_test():
    with tempfile.TemporaryDirectory(prefix="loom-effort-quota-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.send("/approve 1 1\r")
            session.wait("데모 승인 선택 완료")
            line = session.screen.text().splitlines()[0]
            assert line.find("세션") < line.find("환경") and "모델" not in line
            assert "/ 명령" not in session.screen.text() and "도움말" not in line
            session.send("/effort high\r")
            session.wait("● effort high")
            session.command("/chat")
            session.send("할당량 데모\r")
            session.wait("할당량 소진")
            session.wait("tokens 12400")
            session.send("보존할 지시\r")
            session.wait("보존할 지시")
            session.command("/usage")
            session.wait("누적 12.4k")
            session.wait("캐시 7.0k")
            session.send("/usage refresh\r")
            session.resize(80, 40)
            session.wait("사용 가능한 할당량")
            session.wait("일시정지")
        finally:
            session.close()
    print("PTY effort/quota: capability choices, menu hierarchy, reported tokens, quota stop and queue preservation OK", flush=True)


def management_commands_test():
    with tempfile.TemporaryDirectory(prefix="loom-management-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.resize(120, 40)
            session.wait("메시지 입력…")  # Wait for the resized composer before typing.
            session.send("/approve 1 2\r")
            session.wait("데모 승인 선택 완료")
            session.send("/goal set 한국어 목표 관리\r")
            session.wait("한국어 목표 관리")
            session.wait("목표 · active")
            session.send("/goal pause\r")
            session.wait("목표 · paused")
            session.send("/goal resume\r")
            session.wait("목표 · active")
            session.send("/agent list\r")
            session.wait("하위 에이전트")
            session.send("/agent 1 send 간단한 수정 지시\r")
            session.wait("하위 에이전트 요청 처리")
            session.send("/agent 1 stop\r")
            session.wait("중단됨")
            session.send("/skill list\r")
            session.wait("화면 검토 스킬 예시")
            session.send("/skill clear\r")
            session.wait("선택된 스킬을 모두 해제")
            session.command("/compression")
            session.wait("컨텍스트 압축 완료")
            session.send("/queue pause\r")
            session.command("/status")
            session.wait("일시정지")
            session.send("/queue resume\r")
            session.send("/goal clear\r")
            session.wait("목표를 해제했습니다.")
            session.command("/clear")
            session.wait("새 데모 대화")
        finally:
            session.close()
    print("PTY management: goals, child agent controls, skills, compression, queue and clear via keyboard OK", flush=True)


def native_shutdown_test():
    import json
    with tempfile.TemporaryDirectory(prefix="loom-native-shutdown-") as directory:
        root=Path(directory)/"repo"
        root.mkdir()
        marker=Path(directory)/"test.pid"
        subprocess.run(["git","init","-q",str(root)],check=True)
        (root/"Cargo.toml").write_text('[package]\nname="native_shutdown_fixture"\nversion="0.1.0"\nedition="2021"\n[lib]\npath="lib.rs"\n[workspace]\n')
        source='pub fn value() -> u32 { 1 }\n#[cfg(test)] mod tests { #[test] fn wait_for_shutdown() { std::fs::write(r"'+str(marker)+'", std::process::id().to_string()).unwrap(); std::thread::sleep(std::time::Duration::from_secs(30)); } }\n'
        (root/"lib.rs").write_text(source)
        replies=[]
        for name,arguments in [("read",{"path":"lib.rs"}),("edit",{"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"}),("finish",{"summary":"Native pending validation"})]:
            replies.append({"parts":[{"ToolCall":{"id":name,"name":name,"arguments":arguments}}],"usage":{"input":10,"cached_input":0,"output":2},"stop":"ToolUse","model":"scripted","latency":{"secs":0,"nanos":0},"route":None})
        script=Path(directory)/"script.json"
        script.write_text(json.dumps(replies))
        # The fixture reports its pid outside the repository, which the default
        # workspace-write sandbox rightly denies; this test is about cancellation.
        session=Session("--backend","native","--native-sandbox","full-access","--script",str(script),cwd=root)
        closed=False
        group=None
        try:
            session.wait("메시지 입력…")
            session.send("change value\r")
            deadline=time.monotonic()+20
            while not marker.exists() and time.monotonic()<deadline:
                session.pump()
            assert marker.exists(),session.screen.text()
            pid=int(marker.read_text())
            group=os.getpgid(pid)
            assert group!=os.getpgrp()
            session.close()
            closed=True
            state=subprocess.run(["ps","-o","stat=","-p",str(pid)],capture_output=True,text=True).stdout.strip()
            assert not state or state.startswith("Z"),"Native test process survived Ctrl+Q"
            assert (root/"lib.rs").read_text()==source,"Shutdown applied an unvalidated proposal"
        finally:
            if not closed:
                session.close()
            if group is not None:
                try:
                    os.killpg(group,9)
                except ProcessLookupError:
                    pass
    print("PTY Native shutdown: Ctrl+Q cancels validation children and preserves original code OK",flush=True)


def native_stop_report_test():
    with tempfile.TemporaryDirectory(prefix="loom-native-stop-report-") as directory:
        root = Path(directory) / "repo"
        root.mkdir()
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        for name in ["a.rs", "b.rs"]:
            (root / name).write_text("observed evidence\n")
        # Two same-name tools in one batch, then a provider failure. The final
        # report must be produced locally, without a summarizing model reply.
        script = Path(directory) / "script.json"
        script.write_text(json.dumps([{
            "parts": [{"ToolCall": {"id": name, "name": "read", "arguments": {"path": name}}} for name in ["a.rs", "b.rs"]],
            "usage": {"input": 10, "cached_input": 0, "output": 2},
            "stop": "ToolUse", "model": "scripted", "latency": {"secs": 0, "nanos": 0}, "route": None,
        }]))
        session = Session("--backend", "native", "--script", str(script), cwd=root)
        try:
            session.resize(80, 60)
            session.wait("메시지 입력…")
            session.send("fix files\r")
            session.wait("남은 작업", timeout=15)
            for width in [80, 40]:
                session.resize(width, 60)
                session.wait("Core 실행 보고")
                text = session.screen.text()
                for expected in ["read · a.rs", "read · b.rs", "결과:", "원본 반영", "남은 작업", "모델 요청", "scripted model"]:
                    assert expected in text, f"{width}: missing {expected}\n{text}"
            assert (root / "a.rs").read_text() == "observed evidence\n"
            assert (root / "b.rs").read_text() == "observed evidence\n"
        finally:
            session.close()
    print("PTY Native stop report: distinct batched tools, visible results, provider failure and narrow layout OK", flush=True)


def native_plain_question_test():
    with tempfile.TemporaryDirectory(prefix="loom-native-question-") as directory:
        root = Path(directory) / "repo"
        root.mkdir()
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        answer = "Available tools: read, outline, find, edit, write, check, finish."
        script = Path(directory) / "script.json"
        script.write_text(json.dumps([{
            "parts": [{"Text": answer}],
            "usage": {"input": 10, "cached_input": 0, "output": 2},
            "stop": "EndTurn", "model": "scripted", "latency": {"secs": 0, "nanos": 0}, "route": None,
        }]))
        session = Session("--backend", "native", "--script", str(script), cwd=root)
        try:
            session.resize(100, 40)
            session.wait("메시지 입력…")
            session.send("너에게 주어진 도구가 뭔지 말해봐\r")
            session.wait(answer, timeout=15)
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline:
                session.pump()
            text = session.screen.text()
            for wrong in ["작업을 완료하지 못했습니다", "Core 실행 보고", "No file changes"]:
                assert wrong not in text, text
        finally:
            session.close()
    print("PTY Native question: one prose answer, no edit failure or stop report OK", flush=True)


def native_map_progress_test():
    with tempfile.TemporaryDirectory(prefix="loom-native-map-progress-") as directory:
        root = Path(directory) / "repo"
        root.mkdir()
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        (root / "lib.rs").write_text("pub fn sample() {}\n")
        script = Path(directory) / "script.json"
        script.write_text(json.dumps([{
            "parts": [{"ToolCall": {"id": name, "name": name, "arguments": args}}],
            "usage": {"input": 10, "cached_input": 0, "output": 2},
            "stop": "ToolUse", "model": "scripted", "latency": {"secs": 0, "nanos": 0}, "route": None,
        } for name, args in [("map", {"path": "."}), ("finish", {"summary": "구조 확인을 마쳤습니다.", "answer": True})]]))
        session = Session("--backend", "native", "--script", str(script), cwd=root)
        try:
            session.resize(100, 40)
            session.wait("메시지 입력…")
            session.send("저장소 구조를 확인해줘\r")
            session.wait("구조 확인을 마쳤습니다.", timeout=15)
            text = session.screen.text()
            for expected in ["저장소 구조 확인", "파일 1개", "심볼 1개"]:
                assert expected in text, text
            assert "작업 결과 전달" in text, text
            for raw in ["map of the repository", "symbols ranked", "도구 실행 결과:", "finish · finish"]:
                assert raw not in text, text
        finally:
            session.close()
    print("PTY Native map: concise Korean action/counts, internal header hidden OK", flush=True)


def shift_enter_test():
    with tempfile.TemporaryDirectory(prefix="loom-shift-enter-") as directory:
        root = Path(directory) / "repo"
        root.mkdir()
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        script = Path(directory) / "script.json"
        script.write_text(json.dumps([{"parts": [{"Text": "두 줄을 받았습니다."}], "usage": {}, "stop": "EndTurn", "model": "scripted", "latency": {"secs": 0, "nanos": 0}, "route": None}]))
        session = Session("--backend", "native", "--script", str(script), cwd=root)
        try:
            session.wait("메시지 입력…")
            enhancements = [int(bits) for bits in re.findall(rb"\x1b\[>(\d+)u", session.raw)]
            assert any(bits & 1 for bits in enhancements), "terminal was never asked to distinguish Shift+Enter"
            session.send("첫째 줄\x1b[13;2u둘째 줄")
            assert not (root / ".loom/events.jsonl").exists() or "TaskSaved" not in (root / ".loom/events.jsonl").read_text(), "Shift+Enter submitted the draft"
            session.send("\r")
            session.wait("두 줄을 받았습니다.", timeout=15)
            events=[json.loads(line) for line in (root / ".loom/events.jsonl").read_text().splitlines()]
            goals=[(item.get("event", item)).get("TaskSaved", {}).get("goal") for item in events]
            assert "첫째 줄\n둘째 줄" in goals, goals
        finally:
            session.close()
        assert b"\x1b[<1u" in session.raw, "keyboard reporting was not restored"
    print("PTY Shift+Enter: modifier negotiation, newline without submit, restore OK", flush=True)


def native_backend_test():
    import json
    with tempfile.TemporaryDirectory(prefix="loom-native-pty-") as directory:
        root = Path(directory)
        subprocess.run(["git", "init", "-q", directory], check=True)
        (root / "file.txt").write_text("old\n")
        # Keep model fixtures outside the captured workspace.
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as script:
            replies = []
            for name, arguments in [("read", {"path": "file.txt"}), ("edit", {"path": "file.txt", "old": "old", "new": "new"}), ("finish", {"summary": "Native proposal"})]:
                replies.append({"parts": [{"ToolCall": {"id": name, "name": name, "arguments": arguments}}], "usage": {"input": 10, "cached_input": 0, "output": 2}, "stop": "ToolUse", "model": "scripted", "latency": {"secs": 0, "nanos": 0}, "route": None})
            json.dump(replies, script)
            script.flush()
            session = Session("--backend", "native", "--script", script.name, cwd=root)
            try:
                session.wait("메시지 입력…")
                session.send("change file\r")
                session.wait("검증된 Native 변경 승인", timeout=15)
                assert (root / "file.txt").read_text() == "old\n", "Native changed the workspace before approval"
                session.send("\r")
                deadline = time.monotonic() + 10
                while (root / "file.txt").read_text() != "new\n" and time.monotonic() < deadline:
                    session.pump()
                assert (root / "file.txt").read_text() == "new\n", session.screen.text()
                session.wait("메시지 입력…")
            finally:
                session.close()
    print("PTY Native: isolated edit, validated approval, keyboard apply, terminal restore OK", flush=True)


def loom_tool_history_test():
    with tempfile.TemporaryDirectory(prefix="loom-tool-history-") as directory:
        root = Path(directory)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        (root / "source.rs").write_text("pub fn fixture_value() -> i32 { 42 }\n")
        (root / "AGENTS.md").write_text("PRIVATE_INSTRUCTION: inspect without edits.\n")
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json") as script:
            replies = []
            for name, arguments in [
                ("find", {"kind": "symbol", "query": "fixture_value"}),
                ("open", {"target": "source.rs::fixture_value"}),
                ("finish", {"summary": "fixture_value 확인 완료"}),
            ]:
                replies.append({"parts": [{"ToolCall": {"id": name, "name": name, "arguments": arguments}}],
                    "usage": {"input": 10, "cached_input": 0, "output": 2}, "stop": "ToolUse",
                    "model": "PRIVATE_MODEL", "latency": {"secs": 0, "nanos": 0}, "route": None})
            json.dump(replies, script)
            script.flush()
            session = Session("--backend", "loom", "--script", script.name, cwd=root)
            try:
                session.resize(80, 36)
                session.wait("메시지 입력…")
                session.send(":inspect inspect fixture_value\r")
                session.wait("fixture_value 확인 완료", timeout=15)
                session.wait("find · fixture_value")
                session.wait("open · source.rs::fixture_value")
                session.click("find · fixture_value")
                session.wait("검색 · fixture_value")
                session.wait("종류 · symbol")
                # The model remains available in the footer; tool history is quiet.
                text = "\n".join(session.screen.text().splitlines()[:-4])
                for hidden in ("PRIVATE_MODEL", "PRIVATE_INSTRUCTION", '"args"', '"result"', "진행 기록", "실행 상세"):
                    assert hidden not in text, text
            finally:
                session.close()
            session = Session("--backend", "loom", "--script", script.name, cwd=root)
            try:
                session.resize(80, 36)
                session.wait("find · fixture_value")
                session.wait("open · source.rs::fixture_value")
                transcript = "\n".join(session.screen.text().splitlines()[:-4])
                assert "PRIVATE_MODEL" not in transcript
                assert "PRIVATE_INSTRUCTION" not in transcript
            finally:
                session.close()
    print("PTY Loom: tool/input titles, readable expansion and restart history without model or payload noise OK", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--model")
    options = parser.parse_args()
    screen_rgb_sgr_test()
    loom_tool_history_test()
    native_backend_test()
    native_plain_question_test()
    native_map_progress_test()
    shift_enter_test()
    native_stop_report_test()
    native_shutdown_test()
    settings_panel_stability_test()
    checkpoint_responsive_test()
    grouped_settings_test()
    accent_settings_test()
    approval_popup_test()
    approval_keyboard_test()
    session_menu_test()
    demo_test()
    paging_test()
    queue_test()
    force_queue_test()
    force_queue_keyboard_test()
    clipboard_test()
    focus_recovery_test()
    keyboard_only_commands_test()
    management_commands_test()
    local_exploration_test()
    question_form_test()
    escape_interrupt_test()
    activity_graph_test()
    image_attachment_test()
    model_effort_selection_test()
    effort_and_quota_test()
    conversation_dividers_test()
    markdown_test()
    input_history_test()
    rewind_test()
    if options.live:
        live_test(options.model)
