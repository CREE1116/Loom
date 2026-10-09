"""Persist actual interactive choices, restart, override, and recall slash commands."""
import json
import tempfile
from pathlib import Path
from terminal_smoke import Session


def read_choices(root):
    return json.loads((root / '.custom-tui/settings.json').read_text())


def test_preferences():
    with tempfile.TemporaryDirectory(prefix='loom-model-preferences-') as directory:
        root = Path(directory)
        config = root / '.custom-tui'
        config.mkdir()
        (config / 'settings.json').write_text(json.dumps({'model_selections': {
            'v4': {'model': 'backup-model', 'efforts': {'backup-model': 'low'}}}}))
        session = Session('--demo', cwd=root)
        try:
            session.resize(140, 40)
            session.send('/model fixture-fast\r')
            session.wait('선택한 모델 · fixture-fast')
            session.send('/effort high\r')
            session.wait('● effort high')
            session.pump(.3)
            settings = read_choices(root)
            assert settings['model_selections']['demo']['model'] == 'fixture-fast', settings
            assert settings['model_selections']['demo']['efforts']['fixture-fast'] == 'high', settings
            assert settings['model_selections']['v4']['model'] == 'backup-model', settings
            nav = session.screen.text().splitlines()[0]
            assert all(label in nav for label in ['대화', '작업', '세션', '환경', '설정']), nav
            assert '활동' not in nav and '모델' not in nav, nav
            session.send('\x1b[A')
            session.wait('/effort high')
            # Recall is editable input; no model call is started.
            assert '실행 중' not in session.screen.text()
        finally:
            session.close()
        session = Session('--demo', cwd=root)
        try:
            session.resize(140, 40)
            session.send('/model\r')
            session.wait('선택한 모델 · fixture-fast')
            session.wait('● effort high')
            session.send('/model fixture\r')
            session.wait('선택한 모델 · fixture')
            session.send('/effort low\r')
            session.wait('● effort low')
            session.send('/model fixture-fast\r')
            session.wait('선택한 모델 · fixture-fast')
            session.wait('● effort high')
        finally:
            session.close()
        session = Session('--demo', '--model', 'fixture', '--core-effort', 'medium', cwd=root)
        try:
            session.resize(140, 40)
            session.send('/model\r')
            session.wait('선택한 모델 · fixture')
            session.wait('● effort medium')
        finally:
            session.close()
        print('PTY preferences: immediate save, engine isolation, restart, per-model effort, CLI overrides, command recall OK')


if __name__ == '__main__':
    test_preferences()
