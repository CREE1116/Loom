#!/bin/sh
# Install Loom with one command.
#
#   curl -fsSL https://raw.githubusercontent.com/CREE1116/Loom/main/scripts/get.sh | sh
#   (private repository: gh api repos/CREE1116/Loom/contents/scripts/get.sh \
#        -H "Accept: application/vnd.github.raw" | sh)
#
# Apple silicon Macs get the prebuilt release; other systems build from source with
# scripts/install.sh (needs Rust, git, CMake and a C/C++ compiler).
# Options (environment): LOOM_PREFIX (default ~/.local), LOOM_VERSION (default latest),
# LOOM_FROM_SOURCE=1 to always build, LOOM_CUDA=1 to build with CUDA.
set -eu

repo=CREE1116/Loom
prefix=${LOOM_PREFIX:-$HOME/.local}
version=${LOOM_VERSION:-latest}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

say() { printf '%s\n' "$*" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }

# Fetch a release asset: gh works for private repositories, curl for public ones.
fetch_asset() {
    name=$1
    if have gh && gh auth status >/dev/null 2>&1; then
        if [ "$version" = latest ]; then
            gh release download --repo "$repo" --pattern "$name" --dir "$work" --clobber
        else
            gh release download "$version" --repo "$repo" --pattern "$name" --dir "$work" --clobber
        fi
    else
        if [ "$version" = latest ]; then
            url="https://github.com/$repo/releases/latest/download/$name"
        else
            url="https://github.com/$repo/releases/download/$version/$name"
        fi
        curl -fsSL "$url" -o "$work/$name"
    fi
}

install_prebuilt() {
    package=loom-darwin-aarch64
    fetch_asset "$package.tar.gz" || return 1
    fetch_asset "$package.tar.gz.sha256" || return 1
    (cd "$work" && shasum -a 256 -c "$package.tar.gz.sha256" >/dev/null) || {
        say "checksum mismatch for $package.tar.gz"
        return 1
    }
    tar -xzf "$work/$package.tar.gz" -C "$work"
    mkdir -p "$prefix/bin" "$prefix/lib"
    rm -rf "$prefix/lib/loom"
    cp -R "$work/$package/lib/loom" "$prefix/lib/loom"
    cp "$work/$package/bin/loom" "$prefix/bin/.loom.$$"
    mv -f "$prefix/bin/.loom.$$" "$prefix/bin/loom"
    # Downloaded files are quarantined; this build is unsigned.
    xattr -dr com.apple.quarantine "$prefix/bin/loom" "$prefix/lib/loom" 2>/dev/null || true
    "$prefix/bin/loom" --version >/dev/null
}

install_from_source() {
    for tool in git cargo cmake; do
        have "$tool" || { say "building from source needs $tool (Rust: https://rustup.rs)"; exit 1; }
    done
    src=${LOOM_SOURCE_DIR:-$HOME/.local/share/loom/src}
    if [ -d "$src/.git" ]; then
        git -C "$src" pull --ff-only -q
    else
        mkdir -p "$(dirname "$src")"
        if have gh && gh auth status >/dev/null 2>&1; then
            gh repo clone "$repo" "$src" -- -q
        else
            git clone -q "https://github.com/$repo.git" "$src"
        fi
    fi
    if [ "$version" != latest ]; then
        git -C "$src" fetch -q --tags && git -C "$src" checkout -q "$version"
    fi
    set -- --prefix "$prefix"
    [ "${LOOM_CUDA:-0}" = 1 ] && set -- "$@" --cuda
    "$src/scripts/install.sh" "$@"
}

os=$(uname -s)
arch=$(uname -m)
if [ "${LOOM_FROM_SOURCE:-0}" != 1 ] && [ "$os" = Darwin ] && [ "$arch" = arm64 ] && install_prebuilt; then
    say "Installed $prefix/bin/loom"
else
    [ "$os" = Darwin ] && [ "$arch" = arm64 ] && [ "${LOOM_FROM_SOURCE:-0}" != 1 ] &&
        say "No prebuilt package could be fetched; building from source."
    install_from_source
fi
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) say "Add $prefix/bin to PATH, e.g.: echo 'export PATH=\"$prefix/bin:\$PATH\"' >> ~/.zshrc" ;;
esac
say "Start in a project: loom --cwd /path/to/project"
