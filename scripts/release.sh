#!/usr/bin/env bash
# `mise run release` (optionally `-- --publish`): build a release tarball for `tm`, in the same
# layout `.github/workflows/release.yml`'s CI job packages (so a release built here and one built
# in CI install identically via `scripts/install.sh`).
#
# Packages:
#   dist/tm-<target-triple>.tar.gz
#     tm-<target-triple>/bin/tm
#     tm-<target-triple>/share/tm/web/       (the built web client)
#     tm-<target-triple>/README-INSTALL.md
#   dist/tm-<target-triple>.tar.gz.sha256
#
# The version (embedded in README-INSTALL.md, not the filename -- a GitHub release is already
# versioned by its tag) comes from the tm-cli crate (workspace-inherited, so this is really the
# workspace version in Cargo.toml's [workspace.package]). The tarball is refused if it ever
# contains a path matching `.env` or `.tm/` -- see the check below.
#
# --publish additionally runs `gh release create` against this repo's private `origin`
# (alliecatowo/ticket-master). Per this repo's convention, do not run --publish as part of local
# verification: only the orchestrator publishes, from main, after a release-task change like this
# one has been merged. See docs/install.md.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

publish=0
for arg in "$@"; do
  case "$arg" in
    --publish) publish=1 ;;
    *)
      echo "release.sh: unknown argument: $arg" >&2
      exit 1
      ;;
  esac
done

echo "==> Building tm --release ($root)"
cargo build --release -p tm-cli -j 2

echo "==> Building the web client"
if [ ! -f clients/web/pnpm-lock.yaml ]; then
  echo "release.sh: clients/web/pnpm-lock.yaml not found; expected a committed lockfile" >&2
  exit 1
fi
pnpm -C clients/web install --frozen-lockfile
pnpm -C clients/web build

echo "==> Resolving version and target triple"
if command -v jq >/dev/null 2>&1; then
  version="$(cargo metadata --no-deps --format-version 1 |
    jq -r '.packages[] | select(.name=="tm-cli") | .version')"
else
  # jq isn't on PATH; fall back to parsing `cargo pkgid`'s trailing `#version`/`@version`.
  version="$(cargo pkgid -p tm-cli | sed -E 's/.*[#@]//')"
fi
if [ -z "$version" ] || [ "$version" = "null" ]; then
  echo "release.sh: could not determine tm-cli's version" >&2
  exit 1
fi

target_triple="$(rustc -vV | sed -n 's/^host: //p')"
if [ -z "$target_triple" ]; then
  echo "release.sh: could not determine the host target triple from rustc -vV" >&2
  exit 1
fi

bin="target/release/tm"
if [ ! -x "$bin" ]; then
  echo "release.sh: $bin not found or not executable after cargo build --release" >&2
  exit 1
fi

web_dist="clients/web/dist"
if [ ! -f "$web_dist/index.html" ]; then
  echo "release.sh: $web_dist/index.html not found after pnpm build" >&2
  exit 1
fi

name="tm-${target_triple}"
asset="tm-${target_triple}"

echo "==> Staging $name"
stage_root="$(mktemp -d)"
notes_file=""
cleanup() {
  rm -rf "$stage_root"
  [ -n "$notes_file" ] && rm -f "$notes_file"
  return 0
}
trap cleanup EXIT

stage="$stage_root/$name"
mkdir -p "$stage/bin" "$stage/share/tm/web"

cp "$bin" "$stage/bin/tm"
chmod 755 "$stage/bin/tm"
cp -R "$web_dist"/. "$stage/share/tm/web"/

cat >"$stage/README-INSTALL.md" <<EOF
# Installing tm $version

This tarball has two pieces:

    bin/tm              the tm binary
    share/tm/web/        the built web client tm serve looks for next to itself

## Quick install

    mkdir -p "\${PREFIX:-\$HOME/.local}"/bin "\${PREFIX:-\$HOME/.local}"/share/tm
    cp bin/tm "\${PREFIX:-\$HOME/.local}"/bin/tm
    rm -rf "\${PREFIX:-\$HOME/.local}"/share/tm/web
    cp -R share/tm/web "\${PREFIX:-\$HOME/.local}"/share/tm/web
    chmod 755 "\${PREFIX:-\$HOME/.local}"/bin/tm

Make sure \$PREFIX/bin (default \$HOME/.local/bin) is on your \$PATH.

## Verify

    tm --version

See docs/install.md in the ticket-master repository for the full walkthrough (provider setup,
\`tm serve --open\`, Claude Code MCP integration, upgrading, uninstalling).
EOF

mkdir -p dist
tarball="dist/${asset}.tar.gz"
rm -f "$tarball" "${tarball}.sha256"

echo "==> Packaging $tarball"
# COPYFILE_DISABLE keeps macOS's bsdtar from injecting AppleDouble (._*) sidecar entries for
# extended attributes/resource forks the staged files may carry.
(cd "$stage_root" && COPYFILE_DISABLE=1 tar czf "$root/$tarball" "$name")

echo "==> Checking the tarball never carries .env or .tm/ state"
forbidden="$(tar -tzf "$tarball" | grep -E '(^|/)\.env($|\.)|(^|/)\.tm(/|$)' || true)"
if [ -n "$forbidden" ]; then
  echo "release.sh: refusing to ship a tarball containing secret/state paths:" >&2
  echo "$forbidden" >&2
  rm -f "$tarball"
  exit 1
fi

echo "==> Writing checksum"
if command -v shasum >/dev/null 2>&1; then
  (cd dist && shasum -a 256 "$(basename "$tarball")" >"$(basename "$tarball").sha256")
elif command -v sha256sum >/dev/null 2>&1; then
  (cd dist && sha256sum "$(basename "$tarball")" >"$(basename "$tarball").sha256")
else
  echo "release.sh: neither shasum nor sha256sum found" >&2
  exit 1
fi

echo "==> Done"
echo "    version:        $version"
echo "    target triple:  $target_triple"
echo "    tarball:        $tarball ($(du -h "$tarball" | cut -f1))"
echo "    checksum:       ${tarball}.sha256"

if [ "$publish" -eq 1 ]; then
  echo "==> --publish: creating a GitHub release"
  if [ -n "$(git status --porcelain)" ]; then
    echo "release.sh: refusing --publish with a dirty working tree" >&2
    exit 1
  fi
  if ! command -v gh >/dev/null 2>&1; then
    echo "release.sh: gh not found; see docs/install.md for the manual" \
      "'gh release create' command" >&2
    exit 1
  fi
  notes_file="$(mktemp)"
  {
    echo "tm v$version"
    echo
    echo "Prebuilt for $target_triple. See docs/install.md for installation instructions."
  } >"$notes_file"
  gh release create "v$version" "$tarball" "${tarball}.sha256" \
    --repo alliecatowo/ticket-master \
    --target "$(git rev-parse HEAD)" \
    --title "tm v$version" \
    --notes-file "$notes_file"
fi
