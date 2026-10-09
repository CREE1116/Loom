#!/usr/bin/env python3
"""Check that each starter fails behavior tests and its separate reference passes."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent


def command(repo, arguments, env):
    result = subprocess.run(['cargo', *arguments], cwd=repo, env=env,
                            capture_output=True, text=True, timeout=30)
    return result.returncode, result.stdout + result.stderr


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--json', type=Path)
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix='loom-practice-check-') as directory:
        base = Path(directory)
        env = {**os.environ, 'CARGO_TARGET_DIR': str(base/'target'),
               'CARGO_PROFILE_DEV_DEBUG': '0', 'CARGO_PROFILE_TEST_DEBUG': '0',
               'CARGO_INCREMENTAL': '0'}
        for task in sorted((ROOT/'tasks').iterdir()):
            repo = base/task.name
            shutil.copytree(task/'repo', repo)
            (repo/'tests').mkdir(exist_ok=True)
            shutil.copyfile(task/'hidden/hidden.rs', repo/'tests/hidden.rs')
            code, output = command(repo, ['check', '--offline', '--all-targets'], env)
            assert code == 0, f'{task.name}: starter or tests do not compile\n{output}'
            code, output = command(repo, ['test', '--offline', '--test', 'hidden'], env)
            assert code != 0 and 'test result: FAILED' in output, f'{task.name}: hidden tests do not reject the starter\n{output}'
            starter_failures = [line.strip() for line in output.splitlines()
                                if line.startswith('test ') and line.endswith(' ... FAILED')]
            for source in (task/'reference').glob('*.rs'):
                shutil.copyfile(source, repo/'src'/source.name)
            code, output = command(repo, ['test', '--offline'], env)
            assert code == 0, f'{task.name}: reference fails the declared requirements\n{output}'
            result = {'task': task.name, 'starter_compiles': True,
                      'starter_rejected': True, 'starter_failures': starter_failures,
                      'reference_passed': True,
                      'reference_test_results': [line for line in output.splitlines()
                                                 if line.startswith('test result:')]}
            results.append(result)
            print(f'{task.name}: starter rejected; reference and all tests pass', flush=True)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps({'tasks': results, 'external_inference': False},
                                       ensure_ascii=False, indent=2)+'\n')


if __name__ == '__main__':
    main()
