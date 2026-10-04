#!/bin/sh
# scripts/install.sh -- install `tm` from a GitHub release, or from a local release tarball.
#
# Downloading a release asset goes through the `gh` CLI (`gh release download`), which needs a
# login even for public repos. Run `gh auth login` first if `gh auth status` fails below. Release
# assets are public, so you can also `curl` a tarball yourself and pass its path instead.
#
# Usage:
#   scripts/install.sh                    Download the release asset matching this OS/arch from
#                                          the latest GitHub release via `gh release download`.
#   scripts/install.sh <path/to/tarball>  Install from an already-downloaded (or locally built,
#                                          e.g. by `mise run release`) tarball instead. A
#                                          sibling <tarball>.sha256 is required and verified;
#                                          the install fails closed without one.
#
# Env vars:
#   PREFIX   Install prefix (default: $HOME/.local). Installs $PREFIX/bin/tm and
#            $PREFIX/share/tm/web.
#   TM_INSTALL_ALLOW_UNVERIFIED=1
#            Install a tarball that has no .sha256 next to it (you vouch for its provenance).
#
# See docs/install.md for the full walkthrough (provider setup, first run, upgrading,
# uninstalling) and for the `cargo install --git ...` source-install alternative.

set -eu

repo="alliecatowo/ticket-master"
prefix="${PREFIX:-$HOME/.local}"

# Every mktemp -d this script creates gets added here, so a single EXIT trap cleans up all of
# them regardless of which function created which -- functions can't each own an EXIT trap
# without clobbering the ones set by their caller.
cleanup_dirs=""
cleanup() {
  # shellcheck disable=SC2086 # word-splitting is exactly what we want here: a list of dirs.
  if [ -n "$cleanup_dirs" ]; then rm -rf $cleanup_dirs; fi
}
trap cleanup EXIT

log() { printf '%s\n' "$*" >&2; }
die() {
  log "install.sh: $*"
  exit 1
}
need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}
# Sets $new_dir (not command substitution: that would run in a subshell and lose the
# cleanup_dirs update, leaking the directory).
new_tmp_dir() {
  new_dir="$(mktemp -d)"
  cleanup_dirs="$cleanup_dirs $new_dir"
}

usage() {
  cat <<'EOF'
Usage:
  scripts/install.sh                    Download and install the latest release for this OS/arch.
  scripts/install.sh <path/to/tarball>  Install from a local release tarball.

Env:
  PREFIX   install prefix (default: $HOME/.local)
EOF
}

# This repo's release assets are named tm-<target-triple>.tar.gz, where <target-triple> follows
# rustc's own naming (see `rustc -vV`'s "host:" line) -- the same name both `.github/workflows/
# release.yml`'s CI job and `mise run release` (scripts/release.sh) produce, so a downloaded and a
# locally-built tarball install identically. Only the triples that job's matrix (or a local `mise
# run release`) has actually built will exist as real assets; this maps uname's OS/arch spelling
# to that convention so an unsupported combination fails with a clear message rather than a
# confusing download 404.
detect_triple() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *) die "unsupported OS: $os (see docs/install.md for a source install)" ;;
  esac

  case "$arch" in
    arm64 | aarch64) arch_part="aarch64" ;;
    x86_64 | amd64) arch_part="x86_64" ;;
    *) die "unsupported architecture: $arch (see docs/install.md for a source install)" ;;
  esac

  printf '%s-%s\n' "$arch_part" "$os_part"
}

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    die "need shasum or sha256sum to verify a download"
  fi
}

verify_checksum() {
  tarball="$1"
  sha_file="$2"
  expected="$(awk '{print $1}' "$sha_file")"
  [ -n "$expected" ] || die "$sha_file is empty; refusing to install an unverified tarball"
  actual="$(sha256_of "$tarball")"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $tarball: expected $expected, got $actual"
  log "checksum OK ($actual)"
}

# Installs from an on-disk tarball, verifying its checksum first. A missing .sha256 is an error
# (fail closed), not a warning.
install_from_tarball() {
  tarball="$1"
  [ -f "$tarball" ] || die "no such file: $tarball"

  sha_file="${tarball}.sha256"
  if [ -f "$sha_file" ]; then
    verify_checksum "$tarball" "$sha_file"
  elif [ "${TM_INSTALL_ALLOW_UNVERIFIED:-}" = "1" ]; then
    log "warning: no $sha_file next to $tarball; installing UNVERIFIED because TM_INSTALL_ALLOW_UNVERIFIED=1"
  else
    die "no $sha_file next to $tarball; refusing to install an unverified tarball (set TM_INSTALL_ALLOW_UNVERIFIED=1 to override)"
  fi

  new_tmp_dir
  extract_dir="$new_dir"
  tar xzf "$tarball" -C "$extract_dir"

  # The tarball's own top-level dir (tm-<target-triple>), discovered rather than re-derived from the
  # tarball's filename, so this doesn't silently break if the naming convention ever changes.
  top="$(find "$extract_dir" -mindepth 1 -maxdepth 1 -type d | head -n1)"
  [ -n "$top" ] || die "$tarball has no top-level directory"

  # v0.1.0's asset predates the bin/ + share/tm/web/ layout and has the binary directly at
  # <top>/tm; fall back to that so an existing release still installs, not just future ones.
  if [ -x "$top/bin/tm" ]; then
    src_bin="$top/bin/tm"
  elif [ -x "$top/tm" ]; then
    src_bin="$top/tm"
  else
    die "$tarball is missing tm (looked for bin/tm and tm)"
  fi

  mkdir -p "$prefix/bin" "$prefix/share/tm"

  # Write-then-rename rather than overwrite bin/tm in place: replacing a running/just-launched
  # binary's bytes in place (instead of swapping the directory entry) can crash whatever still
  # has the old inode open, and on macOS can trip the code-signature check for a process that is
  # mid-launch.
  tmp_bin="$prefix/bin/.tm.new.$$"
  cp "$src_bin" "$tmp_bin"
  chmod 755 "$tmp_bin"
  mv -f "$tmp_bin" "$prefix/bin/tm"

  if [ -d "$top/share/tm/web" ]; then
    # Vite's asset filenames are content-hashed; a stale share/tm/web left over from a previous
    # version can leave dangling references if merged instead of replaced.
    rm -rf "$prefix/share/tm/web"
    cp -R "$top/share/tm/web" "$prefix/share/tm/web"
  else
    log "warning: $tarball has no share/tm/web (web client); 'tm serve' will fall back to --web-dir/TM_WEB_DIR"
  fi

  log "Installed tm to $prefix/bin/tm"
}

# Verifies the tarball's build provenance attestation (Sigstore, produced by release.yml) with gh.
# Releases up to v0.1.2 were not attested, so a failure is only a warning unless
# TM_REQUIRE_ATTESTATION=1 is set (which makes it fatal).
verify_attestation() {
  if gh attestation verify "$1" --repo "$repo" >/dev/null 2>&1; then
    log "build provenance attestation OK"
  elif [ "${TM_REQUIRE_ATTESTATION:-}" = "1" ]; then
    die "no valid build provenance attestation for $1 (TM_REQUIRE_ATTESTATION=1)"
  else
    log "warning: could not verify a build provenance attestation for $1 (older releases have none); set TM_REQUIRE_ATTESTATION=1 to make this fatal"
  fi
}

download_latest() {
  need_cmd gh
  need_cmd tar
  if ! gh auth status >/dev/null 2>&1; then
    die "gh is not authenticated; run 'gh auth login' first, or download the tarball yourself and pass its path"
  fi

  triple="$(detect_triple)"
  new_tmp_dir
  work="$new_dir"

  log "Looking for the latest ${repo} release asset for ${triple}..."
  if ! gh release download --repo "$repo" --pattern "tm-${triple}.tar.gz*" --dir "$work"; then
    die "no release asset found for ${triple} in ${repo} (no release published yet? see docs/install.md for a source install)"
  fi

  tarball="$(find "$work" -maxdepth 1 -name "tm-${triple}.tar.gz" | head -n1)"
  [ -n "$tarball" ] || die "download succeeded but no tm-${triple}.tar.gz found in $work"

  verify_attestation "$tarball"
  install_from_tarball "$tarball"
}

check_path() {
  case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *)
      log ""
      log "warning: $prefix/bin is not on your PATH. Add this to your shell profile:"
      log "    export PATH=\"$prefix/bin:\$PATH\""
      ;;
  esac
}

print_next_steps() {
  version="$("$prefix/bin/tm" --version 2>/dev/null || printf 'tm')"
  log ""
  log "Installed: $version ($prefix/bin/tm)"
  log ""
  log "Next steps:"
  log "  tm doctor                     # sanity-check your environment"
  log "  tm                            # start the interactive TUI in the current repo"
  log "  tm tickets                    # tickets screen"
  log "  tm serve --open               # HTTP API + web client"
  log "  claude mcp add --transport stdio tm -- tm mcp   # wire tm into Claude Code"
  log ""
  log "See docs/install.md for provider setup (ANTHROPIC_API_KEY, DEVPASS_*, ...), upgrading and"
  log "uninstalling."
}

main() {
  case "${1:-}" in
    -h | --help)
      usage
      exit 0
      ;;
  esac

  case "$#" in
    0) download_latest ;;
    1) install_from_tarball "$1" ;;
    *)
      usage
      exit 1
      ;;
  esac

  check_path
  print_next_steps
}

main "$@"
