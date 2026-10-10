"""Exercise the real TUI/server together in demo mode; no model or source edits.

Build Loom, then run: python3 loom-tui/tests/remote_smoke.py
CUSTOM_TUI_BINARY selects a packaged binary instead of target/debug/loom.
Uses the existing Unix PTY driver from terminal_smoke.py.
"""
import json
import re
import tempfile
import threading
import time
import urllib.error
import urllib.request

from terminal_smoke import Session


def run():
    with tempfile.TemporaryDirectory(prefix="loom-remote-smoke-") as directory:
        session = Session("--demo", cwd=directory)
        try:
            session.resize(100, 40)
            session.command("/remote local")
            session.wait("원격 조작")
            address = re.search(r"http://[^\s]+:(\d+)/t/([A-Z2-7]{26})", session.screen.text())
            assert address, session.screen.text()
            port, token = address.groups()
            origin = f"http://127.0.0.1:{port}"

            def request(path, body=None):
                data = json.dumps(body).encode() if body is not None else None
                headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/json"}
                req = urllib.request.Request(origin + path, data=data, headers=headers)
                outcome = {}

                def fetch():
                    try:
                        with urllib.request.urlopen(req, timeout=8) as response:
                            outcome["value"] = json.load(response)
                    except BaseException as error:
                        outcome["error"] = error

                worker = threading.Thread(target=fetch, daemon=True)
                worker.start()
                while worker.is_alive():
                    session.pump(0.02)
                worker.join()
                if "error" in outcome:
                    raise outcome["error"]
                return outcome["value"]

            def state():
                return request("/api/state?since=0&client=smoke")['state']

            snapshot = state()
            assert isinstance(snapshot['questions'], list)
            for approval in snapshot['approvals']:
                result = request('/api/action', {'action': 'reply', 'id': approval['id'],
                    'choice': approval['choices'][-1]['result'], 'request_id': 'smoke-reply-' + approval['id']})
                assert result['ok']
            session.pump(.4)
            session.send('\x1b')
            session.send('computer draft')
            message = {'action': 'send', 'text': 'phone smoke message', 'request_id': 'smoke-send'}
            assert request('/api/action', message)['ok']
            assert request('/api/action', message)['ok'], 'lost-ACK retry remains accepted'

            deadline = time.monotonic() + 6
            while True:
                snapshot = state()
                entries = [e for e in snapshot['entries'] if e['kind'] == 'user' and e['body'] == message['text']]
                if entries:
                    break
                assert time.monotonic() < deadline, snapshot
                session.pump(.1)
            assert len(entries) == 1, 'retry must not create another message'
            session.wait('computer draft')
            try:
                request('/api/action', {'action': 'model', 'model': 'missing-model', 'request_id': 'smoke-invalid'})
                raise AssertionError('invalid model was accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 409
                assert json.load(error)['error']
            assert request('/api/action', {'action': 'interrupt', 'request_id': 'smoke-stop'})['ok']
            session.send('\x01')
            session.command('/remote off')
            session.wait('원격 조작을 껐습니다')
            try:
                state()
                raise AssertionError('remote server still serves after off')
            except (urllib.error.URLError, ConnectionError):
                pass
            print('remote smoke: real TUI/server, reply, send, deduplicated retry, local draft, rejection, interrupt, off passed')
        finally:
            session.close()


if __name__ == '__main__':
    run()
