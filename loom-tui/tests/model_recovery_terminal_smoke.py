"""A real HTTP 503 after a successful tool, through the router and terminal UI."""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import tempfile
import threading
from terminal_smoke import Session


def test_http_503_recovery():
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, status, body, **headers):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            for name, value in headers.items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            self.reply(200, {'data': [{'id': 'auto'}]})

        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
            number = len(requests)
            if number == 2:
                self.reply(503, {'error': {'code': 503, 'status': 'UNAVAILABLE',
                                         'message': 'Fixture server temporarily busy'}},
                           **{'Retry-After': '2'})
                return
            if number == 1:
                name, args = 'open', {'target': 'README.md'}
            elif number == 3:
                name, args = 'finish', {'summary': 'README.md에서 프로그램 구조를 확인했습니다.'}
            else:
                self.reply(400, {'error': {'message': 'Unexpected extra request'}})
                return
            self.reply(200, {'model': 'auto', 'choices': [{'finish_reason': 'tool_calls',
                'message': {'role': 'assistant', 'content': '', 'tool_calls': [
                    {'id': name, 'type': 'function', 'function': {'name': name,
                     'arguments': json.dumps(args)}}]}}],
                'usage': {'prompt_tokens': 31, 'completion_tokens': 5}},
                **{'x-omniroute-decision': 'provider=gemini;latency_ms=1',
                   'x-omniroute-selected-connection-id': 'fixture-connection'})

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='loom-http-recovery-') as directory:
            repo = Path(directory)/'repo'; repo.mkdir()
            subprocess.run(['git', 'init', '-q', str(repo)], check=True)
            original = 'This fixture program analyzes source architecture.\n'
            (repo/'README.md').write_text(original)
            session = Session('--backend', 'v5', '--model', 'omniroute/auto',
                '--max-steps', '4', '--max-seconds', '20', '--core-attempts', '3', '--core-call-seconds', '5',
                cwd=repo, env={'LOOM_OMNIROUTE': '1',
                    'LOOM_OMNIROUTE_URL': f'http://127.0.0.1:{server.server_port}/v1',
                    'OMNIROUTE_API_KEY': 'loom-fixture-only',
                    'OMNIROUTE_MANAGE_KEY': 'loom-fixture-only',
                    'LOOM_MODEL_STATE_DIR': str(Path(directory)/'model-state'),
                    'LOOM_SETTINGS_STATE_DIR': str(Path(directory)/'settings-state')})
            try:
                session.resize(140, 45); session.wait('메시지 입력', timeout=15)
                session.send('디렉토리를 보고 프로그램을 설명해줘\r')
                session.wait('연결 재시도 대기', timeout=15)
                session.send('/trace\r')
                session.wait('재시도 예약', timeout=5)
                progress = session.screen.text()
                assert 'README.md' in progress and '2/3' in progress, progress
                assert '503' in progress, progress
                print('HTTP 503 PTY: reconnecting after successful README read', flush=True)
                print(progress, flush=True)
                session.send('/chat\r')
                session.wait('README.md에서 프로그램 구조를 확인했습니다.', timeout=15)
                screen = session.screen.text()
                assert (repo/'README.md').read_text() == original
                assert len(requests) == 3, requests
                assert requests[1] == requests[2], 'Failed request was not resumed unchanged'
                event_files = list((repo/'.loom/v5/tasks').glob('*.events.jsonl'))
                assert len(event_files) == 1, event_files
                events = [json.loads(line) for line in event_files[0].read_text().splitlines()]
                succeeded = [e for e in events if e['detail'].get('completed')]
                assert sum(e['detail'].get('tool') == 'open' for e in succeeded) == 1, succeeded
                issues = [e for e in events if e['target'] == 'model_retry']
                assert len(issues) == 1 and issues[0]['detail']['next_attempt'] == 2, issues
                assert '503' in issues[0]['detail']['reason'], issues
                assert list((Path(directory)/'model-state/cache').glob('omniroute-models-*.json'))
                assert (Path(directory)/'model-state/model-usage.jsonl').exists()
                print('HTTP 503 PTY: recovered, same request, one read, retained cause, unchanged original', flush=True)
                print(screen, flush=True)
            finally:
                session.close()
    finally:
        server.shutdown(); server.server_close(); thread.join(timeout=2)


if __name__ == '__main__':
    test_http_503_recovery()
