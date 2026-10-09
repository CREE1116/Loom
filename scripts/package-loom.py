#!/usr/bin/env python3
"""Build a self-contained Loom binary package."""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--install", action="store_true", help="also install loom in --bin-dir")
    parser.add_argument("--bin-dir", type=Path, default=Path.home() / ".local/bin")
    parser.add_argument("--cuda", action="store_true", help="build the native runtime with CUDA (requires the CUDA toolkit)")
    parser.add_argument("--no-build", action="store_true", help="package the existing release binary")
    args = parser.parse_args()
    command = ["cargo", "build", "--release", "--locked", "--manifest-path", str(ROOT / "loom-tui/Cargo.toml"), "--bin", "loom"]
    if args.cuda:
        command += ["--features", "cuda"]
    if not args.no_build:
        subprocess.run(command, check=True)
    name = "loom.exe" if os.name == "nt" else "loom"
    binary = ROOT / "loom-tui/target/release" / name
    if not binary.is_file():
        parser.error(f"release executable does not exist: {binary}")
    architecture = {"arm64": "aarch64", "AMD64": "x86_64"}.get(platform.machine(), platform.machine())
    package_name = f"loom-{platform.system().lower()}-{architecture}" + ("-cuda" if args.cuda else "")
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    archive = output / (package_name + ".tar.gz")
    with tempfile.TemporaryDirectory(prefix="loom-package-") as directory:
        package = Path(directory) / package_name
        package.mkdir()
        shutil.copy2(binary, package / name)
        (package / "README.txt").write_text(
            "Loom\n\nCopy the loom executable into a directory on PATH.\n"
            "Start: loom --cwd /path/to/project\n"
            "Try without inference: loom --demo\n"
            "Register providers: loom provider --help (or /provider inside Loom)\n"
            "Settings, credentials and installed models remain in ~/.loom.\n"
            "The coding Core and local inference runtime are in this executable.\n"
            "Native inference: loom model pull hf:owner/repo/file.gguf; choose loom/MODEL in /model.\n"
            "Uninstall: remove the executable; user data is preserved.\n", encoding="utf-8")
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(package, arcname=package_name)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum = archive.with_suffix(archive.suffix + ".sha256")
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    # Inspect packaged executable before installing anything.
    with tempfile.TemporaryDirectory(prefix="loom-package-check-") as directory:
        with tarfile.open(archive) as tar:
            tar.extractall(directory, filter="data")
        packaged = Path(directory) / package_name / name
        subprocess.run([str(packaged), "--version"], check=True)
        subprocess.run([str(packaged), "provider", "--help"], check=True, stdout=subprocess.DEVNULL)
        subprocess.run([str(packaged), "--self-check"], check=True, stdout=subprocess.DEVNULL)
    if args.install:
        args.bin_dir.mkdir(parents=True, exist_ok=True)
        destination = args.bin_dir / name
        with tempfile.NamedTemporaryFile(dir=args.bin_dir, delete=False) as staged:
            staged_path = Path(staged.name)
        try:
            shutil.copy2(binary, staged_path)
            staged_path.chmod(0o755)
            staged_path.replace(destination)
        finally:
            staged_path.unlink(missing_ok=True)
        print(f"Installed: {destination}")
        if str(args.bin_dir.resolve()) not in os.environ.get("PATH", "").split(os.pathsep):
            print(f"Add {args.bin_dir.resolve()} to PATH, or invoke the installed file directly.")
    print(f"Package: {archive}\nSHA256: {checksum}")


if __name__ == "__main__":
    main()
