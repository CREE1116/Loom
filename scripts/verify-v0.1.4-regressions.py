"""CI-only negative control: new checks must reject the affected v0.1.3 code."""
import json
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
BASE = "v0.1.3"


def run(args):
    return subprocess.run(args, cwd=ROOT, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE)


def baseline(files, command, verify):
    saved = {name: (ROOT / name).read_bytes() for name in files}
    try:
        for name in files:
            source = subprocess.check_output(["git", "show", f"{BASE}:{name}"], cwd=ROOT)
            if name == "loom-core/src/agent/mod.rs":
                # Keep newly added attachment API fields so the current tests compile.
                source = saved[name].replace(b"tokens: 0,", b"tokens: 160000,", 1)
            (ROOT / name).write_bytes(source)
        result = run(command)
        assert result.returncode != 0, "regression checks incorrectly accept v0.1.3"
        verify(result.stdout)
    finally:
        for name, content in saved.items():
            (ROOT / name).write_bytes(content)


def core_failure(output):
    for name in ["default_token_budget_allows_progress_after_160k_and_explicit_budget_still_stops",
                 "empty_search_scope_means_repository_and_named_scope_stays_restricted",
                 "opening_short_files_clamps_end_and_reports_actual_range"]:
        assert f"test {name} ... FAILED" in output, output[-12000:]
        print(f"Expected baseline failure: {name}")


def ui_failure(output):
    report = json.loads(output)
    failures = [check for check in report["checks"] if not check["pass"]]
    assert len(failures) == 6, failures
    assert all(check["scenario"] == "models" for check in failures), failures
    for check in failures:
        print(f"Expected baseline failure: {check['name']}")


def image_failure(output):
    name = "loom_accepts_clipboard_images_with_text_and_image_only_without_leaking_to_next_turn"
    assert f"test {name} ... FAILED" in output, output[-12000:]
    print(f"Expected baseline failure: {name}")


subprocess.run(["git", "fetch", "--depth", "1", "origin", "tag", BASE], cwd=ROOT, check=True)
baseline(["loom-core/src/agent/mod.rs", "loom-core/src/agent/worker.rs",
          "loom-core/src/agent/query.rs", "loom-core/src/agent/tools.rs",
          "loom-core/src/effects/overlay.rs"],
         ["cargo", "test", "--locked", "--manifest-path", "loom-core/Cargo.toml",
          "--test", "agent_runtime", "--", "--test-threads=1"], core_failure)
baseline(["loom-tui/src/app/events.rs"],
         ["cargo", "run", "--locked", "--quiet", "--manifest-path", "loom-tui/Cargo.toml",
          "--", "--self-check", "--json"], ui_failure)
baseline(["loom-tui/src/backend/versions.rs"],
         ["cargo", "test", "--locked", "--manifest-path", "loom-tui/Cargo.toml",
          "--test", "versions", "loom_accepts_clipboard_images", "--", "--test-threads=1"], image_failure)
print("All new regression checks reject v0.1.3; current sources restored.")
