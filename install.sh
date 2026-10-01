#!/bin/sh
# Install hopper for the current user.
#
#   curl -fsSL https://raw.githubusercontent.com/ptMuta/hopper/main/install.sh | sh
#
# Puts the binary in ~/.local/bin, makes sure that is on PATH, and sets up shell completion
# for bash, zsh and fish. Safe to re-run: it replaces its own shell-config block rather than
# adding another. Update later with `hopper self-update`.
#
# Options: see say_help below, or run with --help.

set -eu

REPO="ptMuta/hopper"

say_help() {
    cat <<'HELP'
Install hopper for the current user, into ~/.local/bin.

Options (or the environment variable beside each):
  --version vX.Y.Z   HOPPER_VERSION            a specific release instead of the latest
  --bin-dir DIR      HOPPER_BIN_DIR            where to put the binary [~/.local/bin]
  --no-modify-path   HOPPER_NO_MODIFY_PATH=1   leave shell startup files alone
  --from FILE        HOPPER_FROM               install from a local release tarball
HELP
}
VERSION="${HOPPER_VERSION:-latest}"
BIN_DIR="${HOPPER_BIN_DIR:-$HOME/.local/bin}"
MODIFY_PATH=1
[ "${HOPPER_NO_MODIFY_PATH:-}" = 1 ] && MODIFY_PATH=0
FROM="${HOPPER_FROM:-}"

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --bin-dir) BIN_DIR="$2"; shift 2 ;;
        --no-modify-path) MODIFY_PATH=0; shift ;;
        --from) FROM="$2"; shift 2 ;;
        -h|--help) say_help; exit 0 ;;
        *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
    esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- platform

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64|Linux-amd64) TARGET="x86_64-linux" ;;
    *) die "no release is published for $(uname -s) $(uname -m); build from source: https://github.com/$REPO" ;;
esac

# ---------------------------------------------------------------- tools

# Only needed when downloading, so --from works on a machine with neither.
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --https-only -O "$2" "$1"
    else
        die "needs curl or wget to download hopper"
    fi
}

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "needs sha256sum or shasum to verify the download"
fi

command -v tar >/dev/null 2>&1 || die "needs tar"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

# ---------------------------------------------------------------- download and verify

if [ -n "$FROM" ]; then
    [ -f "$FROM" ] || die "$FROM does not exist"
    TARBALL="$FROM"
    say "Installing from $FROM"
else
    if [ "$VERSION" = latest ]; then
        BASE="https://github.com/$REPO/releases/latest/download"
    else
        case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
        BASE="https://github.com/$REPO/releases/download/$VERSION"
    fi

    fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "could not download SHA256SUMS from $BASE"
    # The checksum file names the asset, which carries the version: no API call needed.
    LINE="$(grep -E "hopper-v[0-9.]+-$TARGET\.tar\.gz\$" "$TMP/SHA256SUMS" | head -n 1)"
    [ -n "$LINE" ] || die "the release has no build for $TARGET"
    EXPECTED="${LINE%% *}"
    NAME="${LINE##* }"
    NAME="${NAME#\*}"

    say "Downloading $NAME"
    fetch "$BASE/$NAME" "$TMP/$NAME" || die "could not download $NAME"
    ACTUAL="$(sha256 "$TMP/$NAME")"
    [ "$ACTUAL" = "$EXPECTED" ] ||
        die "checksum mismatch for $NAME: expected $EXPECTED, got $ACTUAL"
    TARBALL="$TMP/$NAME"
fi

mkdir -p "$TMP/unpack"
tar -xzf "$TARBALL" -C "$TMP/unpack"
NEW="$(find "$TMP/unpack" -type f -name hopper | head -n 1)"
[ -n "$NEW" ] || die "the archive does not contain a hopper binary"

# ---------------------------------------------------------------- install

mkdir -p "$BIN_DIR"
# Copy beside, then rename: a running hopper keeps working, and an interrupted copy never
# replaces a good binary with half of one.
cp "$NEW" "$BIN_DIR/.hopper.new"
chmod 755 "$BIN_DIR/.hopper.new"
mv -f "$BIN_DIR/.hopper.new" "$BIN_DIR/hopper"
HOPPER="$BIN_DIR/hopper"
say "Installed $("$HOPPER" --version) to $HOPPER"

# ---------------------------------------------------------------- completions

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"

# bash-completion loads this directory on demand, so nothing needs adding to .bashrc.
# Written to a temporary file first: a release too old to generate completions must not leave
# an empty or broken script behind.
completion() {
    shell="$1"
    dest="$2"
    if "$HOPPER" completions "$shell" > "$TMP/completion" 2>/dev/null && [ -s "$TMP/completion" ]; then
        mkdir -p "$(dirname "$dest")"
        cp "$TMP/completion" "$dest"
        DONE="${DONE:+$DONE, }$shell"
    fi
}
DONE=""
ZSH_FUNCS="$DATA/zsh/site-functions"
completion bash "$DATA/bash-completion/completions/hopper"
if command -v zsh >/dev/null 2>&1 || [ -f "$HOME/.zshrc" ]; then
    completion zsh "$ZSH_FUNCS/_hopper"
fi
if command -v fish >/dev/null 2>&1 || [ -d "$CONFIG/fish" ]; then
    completion fish "$CONFIG/fish/completions/hopper.fish"
fi
if [ -n "$DONE" ]; then
    say "Completions installed for $DONE"
else
    say "This release cannot generate shell completions; skipped"
fi

# ---------------------------------------------------------------- shell startup files

BEGIN="# >>> hopper >>>"
END="# <<< hopper <<<"

# Replace this script's block in a file, or append one. Never touches anything else.
write_block() {
    file="$1"
    body="$2"
    mkdir -p "$(dirname "$file")"
    touch "$file"
    awk -v b="$BEGIN" -v e="$END" '$0==b{skip=1} !skip{print} $0==e{skip=0}' "$file" > "$file.hopper-tmp"
    if [ -n "$body" ]; then
        # A blank line before the block, unless the file is empty or already ends in one.
        if [ -s "$file.hopper-tmp" ] && [ -n "$(tail -n 1 "$file.hopper-tmp")" ]; then
            printf '\n' >> "$file.hopper-tmp"
        fi
        printf '%s\n%s\n%s\n' "$BEGIN" "$body" "$END" >> "$file.hopper-tmp"
    fi
    # cat rather than mv, so a symlinked dotfile stays a symlink and keeps its permissions.
    cat "$file.hopper-tmp" > "$file"
    rm -f "$file.hopper-tmp"
}

case ":$PATH:" in
    *":$BIN_DIR:"*) ON_PATH=1 ;;
    *) ON_PATH=0 ;;
esac

if [ "$MODIFY_PATH" = 1 ]; then
    if [ "$ON_PATH" = 0 ]; then
        PATH_LINE="case \":\$PATH:\" in *\":$BIN_DIR:\"*) ;; *) export PATH=\"$BIN_DIR:\$PATH\" ;; esac"
    else
        PATH_LINE=""
    fi

    # bash: interactive shells read .bashrc; login shells usually source it from .profile.
    if [ -n "$PATH_LINE" ]; then
        write_block "$HOME/.bashrc" "$PATH_LINE"
        write_block "$HOME/.profile" "$PATH_LINE"
    fi

    # zsh: completion has to be registered after compinit, which .zshrc may or may not run.
    if [ -f "$ZSH_FUNCS/_hopper" ]; then
        ZSH_BODY="fpath=(\"$ZSH_FUNCS\" \$fpath)
if (( \$+functions[compdef] )); then autoload -Uz _hopper && compdef _hopper hopper; fi"
        [ -n "$PATH_LINE" ] && ZSH_BODY="$PATH_LINE
$ZSH_BODY"
        write_block "$HOME/.zshrc" "$ZSH_BODY"
    fi

    # fish: conf.d is read on every start; completions/ needs nothing.
    if [ -n "$PATH_LINE" ] && { command -v fish >/dev/null 2>&1 || [ -d "$CONFIG/fish" ]; }; then
        mkdir -p "$CONFIG/fish/conf.d"
        printf '# Added by hopper install.sh\nfish_add_path -g "%s"\n' "$BIN_DIR" > "$CONFIG/fish/conf.d/hopper.fish"
    fi
fi

# ---------------------------------------------------------------- done

say ""
if [ "$ON_PATH" = 1 ]; then
    say "Done. Try: hopper --help"
elif [ "$MODIFY_PATH" = 1 ]; then
    say "Done. $BIN_DIR was added to PATH for new shells; for this one, run:"
    say "  export PATH=\"$BIN_DIR:\$PATH\""
else
    say "Done. $BIN_DIR is not on PATH; add it yourself, or run $HOPPER directly."
fi
say "Update later with: hopper self-update"
