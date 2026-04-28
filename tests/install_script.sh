#!/bin/bash
# Regression for ./install's atomic-install function (yggdrasil-175).
# Sources install_atomic, runs it against a fake binary, asserts the
# target exists + is executable. The full SIGKILL repro requires a
# real `ygg` previously cached by Gatekeeper, so it stays manual.
set -uo pipefail

HERE="$(cd "$(dirname "$0")"/.. && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Pull the prelude + function only (everything up to the `echo
# "Installing ygg ..."` line). We can't `source` the full script
# because it'd kick off a real install.
awk '
  /^echo "Installing ygg/ { exit }
  { print }
' "$HERE/install" > "$TMP/install_lib.sh"

# Pretend we're on darwin so the codesign branch runs.
PREFIX="$TMP/prefix"
OS="darwin"
ARCH="x86_64"
TARGET="x86_64-apple-darwin"
mkdir -p "$PREFIX"

# A fake "ygg" binary that prints "ygg 0.1.0" (verify smoke needs that
# exact prefix to succeed).
cat > "$TMP/fake_src" <<'EOF'
#!/bin/bash
echo "ygg 0.1.0"
EOF
chmod +x "$TMP/fake_src"

# Source the prelude + function (drop set -e so the function's own
# error handling is what we observe).
# shellcheck disable=SC1090
source "$TMP/install_lib.sh"

install_atomic "$TMP/fake_src"

[[ -x "$PREFIX/ygg" ]] || { echo "FAIL: target not executable" >&2; exit 1; }
[[ ! -e "$PREFIX/ygg.next" ]] || { echo "FAIL: ygg.next leaked" >&2; exit 1; }
out="$("$PREFIX/ygg" --version)"
[[ "$out" == "ygg 0.1.0" ]] || { echo "FAIL: --version output: $out" >&2; exit 1; }

echo "ok: install_atomic atomic-mv + codesign + verify path"
