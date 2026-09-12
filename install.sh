#!/bin/sh
# Install the `theta` CLI.
#
#   curl -fsSL https://thetabase.co/install.sh | sh
#
# POSIX sh, not bash: this runs on whatever the machine has, including Alpine
# containers and minimal CI images where /bin/sh is not bash.
#
# What this deliberately does not do: install to a system directory, use sudo,
# or edit shell profiles. A script piped from the internet into a shell has
# already asked for a lot of trust, and quietly editing ~/.bashrc is more than
# it needs. It installs to a user-writable directory and tells you if that
# directory is not on your PATH.

set -eu

REPO="${THETA_REPO:-ThetaBase/thetabase}"
VERSION="${THETA_VERSION:-latest}"
INSTALL_DIR="${THETA_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "this script needs \`$1\` and cannot find it"
}

need uname
need mktemp
need tar

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
    fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
    fetch_stdout() { wget -qO- "$1"; }
else
    die "this script needs \`curl\` or \`wget\` and cannot find either"
fi

# ---- what are we running on -------------------------------------------------

os=$(uname -s)
arch=$(uname -m)

case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    # Told plainly rather than guessed at. A wrong binary that downloads and
    # then will not run is worse than a clear refusal.
    MINGW*|MSYS*|CYGWIN*)
        die "on Windows, download the .zip from https://github.com/$REPO/releases" ;;
    *) die "unsupported operating system: $os" ;;
esac

case "$arch" in
    x86_64|amd64) arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *) die "unsupported architecture: $arch" ;;
esac

target="${arch_part}-${os_part}"

# ---- which version ----------------------------------------------------------

if [ "$VERSION" = "latest" ]; then
    VERSION=$(
        fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" |
        tr ',' '\n' | grep '"tag_name"' | head -n 1 |
        sed 's/.*"tag_name" *: *"\([^"]*\)".*/\1/'
    ) || die "could not reach the GitHub release API"
    [ -n "$VERSION" ] || die "could not determine the latest version; set THETA_VERSION"
fi

archive="theta-${VERSION}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${VERSION}/${archive}"

say "Installing theta ${VERSION} (${target})"

# ---- download and verify ----------------------------------------------------

tmp=$(mktemp -d)
# Cleans up on failure too, which matters because the checks below exit.
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch "$url" "$tmp/$archive" || die "could not download $url"

# Verified when a checksum is published and when a tool exists to check it.
# Skipped loudly rather than silently: somebody piping this into a shell should
# know which of the two happened.
if fetch "$url.sha256" "$tmp/$archive.sha256" 2>/dev/null; then
    if command -v shasum >/dev/null 2>&1; then
        ( cd "$tmp" && shasum -a 256 -c "$archive.sha256" >/dev/null ) \
            || die "checksum mismatch — the download is not what was published"
    elif command -v sha256sum >/dev/null 2>&1; then
        ( cd "$tmp" && sha256sum -c "$archive.sha256" >/dev/null ) \
            || die "checksum mismatch — the download is not what was published"
    else
        say "  warning: no shasum or sha256sum, so the download was not verified"
    fi
else
    say "  warning: no published checksum for this release, so nothing was verified"
fi

# ---- install ----------------------------------------------------------------

tar -xzf "$tmp/$archive" -C "$tmp"
unpacked="$tmp/theta-${VERSION}-${target}"
[ -f "$unpacked/theta" ] || die "the archive did not contain a \`theta\` binary"

mkdir -p "$INSTALL_DIR"

# Each binary is written to a temporary name and moved into place, so an
# interrupted install cannot leave a half-written file where a working one
# used to be.
install_one() {
    cp "$unpacked/$1" "$INSTALL_DIR/$1.tmp"
    chmod +x "$INSTALL_DIR/$1.tmp"
    mv "$INSTALL_DIR/$1.tmp" "$INSTALL_DIR/$1"
}

install_one theta
say "Installed to $INSTALL_DIR/theta"

# `theta-mcp` is how an agent reaches the database, and the documentation tells
# people to run it. Optional here rather than required, so an older archive
# that predates it still installs the CLI instead of failing outright.
if [ -f "$unpacked/theta-mcp" ]; then
    install_one theta-mcp
    say "Installed to $INSTALL_DIR/theta-mcp"
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH. Add it:"
        say ""
        say "  export PATH=\"\$PATH:$INSTALL_DIR\""
        say ""
        ;;
esac

say ""
say "Next:"
say "  theta login github"
say "  theta use <org> <project> --create"
say "  theta demo"
