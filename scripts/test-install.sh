#!/bin/sh
# Regression test for scripts/install.sh integrity checks. Builds a tiny fake release tarball and
# asserts: install fails without a .sha256, fails on a wrong one, succeeds with a right one, and
# the override env var works. Run: sh scripts/test-install.sh
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/pkg/tm-test/bin"
printf '#!/bin/sh\necho tm-test\n' >"$work/pkg/tm-test/bin/tm"
chmod 755 "$work/pkg/tm-test/bin/tm"
tar czf "$work/tm-test.tar.gz" -C "$work/pkg" tm-test

sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | awk '{print $1}'; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# 1. no checksum file -> refused, nothing installed
if PREFIX="$work/p1" sh "$here/install.sh" "$work/tm-test.tar.gz" >/dev/null 2>&1; then fail "installed without a .sha256"; fi
[ ! -e "$work/p1/bin/tm" ] || fail "binary present after refused install"

# 2. wrong checksum -> refused
echo "0000000000000000000000000000000000000000000000000000000000000000  tm-test.tar.gz" >"$work/tm-test.tar.gz.sha256"
if PREFIX="$work/p2" sh "$here/install.sh" "$work/tm-test.tar.gz" >/dev/null 2>&1; then fail "installed with a wrong checksum"; fi

# 3. empty checksum file -> refused
: >"$work/tm-test.tar.gz.sha256"
if PREFIX="$work/p3" sh "$here/install.sh" "$work/tm-test.tar.gz" >/dev/null 2>&1; then fail "installed with an empty checksum file"; fi

# 4. right checksum -> installed
echo "$(sha "$work/tm-test.tar.gz")  tm-test.tar.gz" >"$work/tm-test.tar.gz.sha256"
PREFIX="$work/p4" sh "$here/install.sh" "$work/tm-test.tar.gz" >"$work/out4" 2>&1 || { cat "$work/out4" >&2; fail "valid install failed"; }
[ -x "$work/p4/bin/tm" ] || fail "binary missing after valid install"

# 5. explicit override with no checksum file
rm "$work/tm-test.tar.gz.sha256"
TM_INSTALL_ALLOW_UNVERIFIED=1 PREFIX="$work/p5" sh "$here/install.sh" "$work/tm-test.tar.gz" >/dev/null 2>&1 || fail "override install failed"

echo "install.sh integrity checks OK"
