#!/bin/sh
# Build Loom from this checkout and install the `loom` command.
#
#   scripts/install.sh                 # ~/.local/bin/loom
#   scripts/install.sh --prefix /opt/loom
#   scripts/install.sh --cuda          # Linux/Windows with the CUDA toolkit
#   scripts/install.sh --no-mlx        # macOS: skip the MLX engine
#
# Requires Rust (https://rustup.rs), git, cmake and a C/C++ compiler. On Apple silicon the
# MLX engine is downloaded with python3/pip unless --no-mlx is given. User data (models,
# providers, settings) lives in ~/.loom and is never touched by installing or removing.
set -eu

prefix="$HOME/.local"
cuda=0
mlx=1
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) prefix=$2; shift 2 ;;
        --prefix=*) prefix=${1#--prefix=}; shift ;;
        --cuda) cuda=1; shift ;;
        --no-mlx) mlx=0; shift ;;
        -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
command -v cargo >/dev/null 2>&1 || { echo "cargo not found: install Rust from https://rustup.rs" >&2; exit 1; }
command -v cmake >/dev/null 2>&1 || { echo "cmake not found: it builds the bundled llama.cpp (brew/apt install cmake)" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
if [ "$os" = Darwin ] && [ "$arch" = arm64 ] && [ "$mlx" = 1 ] && [ ! -f "$root/loom-core/vendor/mlx/lib/libmlx.dylib" ]; then
    if command -v python3 >/dev/null 2>&1; then
        echo "Fetching the MLX runtime (Apple silicon)…"
        "$root/scripts/fetch-mlx" || echo "MLX download failed; continuing without the MLX engine" >&2
    else
        echo "python3 not found; building without the MLX engine" >&2
    fi
fi
if [ "$mlx" = 0 ]; then
    # build.rs looks for the runtime here; point it elsewhere to build without MLX.
    export LOOM_MLX_DIR="$root/loom-core/vendor/none"
fi

features=""
[ "$cuda" = 1 ] && features="--features cuda"
echo "Building Loom (release)…"
# shellcheck disable=SC2086
cargo build --release --locked --manifest-path "$root/loom-tui/Cargo.toml" --bin loom $features

binary="$root/loom-tui/target/release/loom"
mkdir -p "$prefix/bin"
staged="$prefix/bin/.loom.$$"
cp "$binary" "$staged"
chmod 755 "$staged"

if [ "$os" = Darwin ]; then
    linked=$(otool -L "$staged" | awk '/libmlx\.dylib/ {print $1}')
    if [ -n "$linked" ]; then
        # Ship the MLX library (and its Metal kernels) next to the install.
        mkdir -p "$prefix/lib/loom"
        cp -R "$(dirname "$linked")/." "$prefix/lib/loom/"
        install_name_tool -id "@rpath/libmlx.dylib" "$prefix/lib/loom/libmlx.dylib"
        install_name_tool -change "$linked" "@executable_path/../lib/loom/libmlx.dylib" "$staged"
        codesign --force --sign - "$prefix/lib/loom/libmlx.dylib" 2>/dev/null || true
        codesign --force --sign - "$staged" 2>/dev/null || true
    fi
fi

mv -f "$staged" "$prefix/bin/loom"
"$prefix/bin/loom" --version >/dev/null
echo "Installed $prefix/bin/loom"
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "Add $prefix/bin to PATH, e.g.: echo 'export PATH=\"$prefix/bin:\$PATH\"' >> ~/.zshrc" ;;
esac
echo "Start in a project:  loom --cwd /path/to/project   (try without a model: loom --demo)"
