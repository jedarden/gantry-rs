#!/bin/sh
# gantry installer — the curl-pipe entry point (plan §8 "doctor / installer":
# install.sh + `gantry uninstall`; Phase 3 acceptance: "curl-pipe install on
# a fresh box reaches a passing quickcheck with zero config").
#
#   curl -fsSL https://git.ardenone.com/jedarden/gantry-rs/raw/branch/main/install.sh | sh
#
# Verbs:
#   install    Download the static musl release, lay the binary + cargo shim,
#              finish with `gantry quickcheck` (the acceptance gate). This is
#              the default when no verb is given, so the bare curl-pipe above
#              is a complete install.
#   uninstall  Delegate to `gantry uninstall` — the binary reverses the whole
#              layout (shims, binary, state, config, slice unit); the script
#              only locates the binary and relays its exit code.
#   doctor     Delegate to `gantry doctor` the same way.
#
# What install deliberately does NOT do: write any configuration. Tier-0
# zero-config mode is the acceptance contract — the install must work with
# nothing under ~/.config/gantry, and it does, by construction (quickcheck is
# the Tier-0 proof). The gantry.slice systemd unit is provisioned by the
# binary itself, idempotently, on first capped run — not by this script.
#
# Environment overrides (all optional; the overrides exist so an air-gapped
# box can install from a staged file and so the hermetic integration test can
# exercise this exact script without the network):
#   GANTRY_INSTALL_FORGE      Forgejo base URL (default https://git.ardenone.com)
#   GANTRY_INSTALL_REPO       owner/name (default jedarden/gantry-rs)
#   GANTRY_INSTALL_VERSION    release tag (default: the forge's latest release)
#   GANTRY_BIN_DIR            install dir (default $HOME/.local/bin) — holds
#                             both the gantry binary and the cargo shim
#   GANTRY_INSTALL_LOCAL_BIN  path to a prebuilt gantry binary: install a copy
#                             of it instead of downloading the release tarball
#
# Safety rule (the mirror of uninstall.rs's): install never clobbers a
# foreign `cargo`. A shim that is provably gantry's — a symlink onto the
# gantry binary, or a byte-identical copy of it (the layout uninstall.rs also
# accepts) — is replaced in place; anything else named `cargo` in the install
# dir belongs to someone else's toolchain and the install refuses, naming the
# escape hatch (--force moves it to cargo.pre-gantry.bak — kept, not deleted;
# the same .bak convention the plan's migration section uses for the bash
# shims gantry replaces). A fresh box has no cargo there, so the acceptance
# path never needs it.

set -u

FORGE="${GANTRY_INSTALL_FORGE:-https://git.ardenone.com}"
REPO="${GANTRY_INSTALL_REPO:-jedarden/gantry-rs}"
BIN_DIR="${GANTRY_BIN_DIR:-$HOME/.local/bin}"
FORCE=0

err() { printf 'install.sh: %s\n' "$1" >&2; }
info() { printf '[gantry] install: %s\n' "$1"; }

usage() {
  cat >&2 <<'EOF'
usage: install.sh [install [--force] | uninstall [-- <gantry uninstall args…>]
                 | doctor [-- <gantry doctor args…>]]

  install     fetch the release, lay the binary + cargo shim, run quickcheck
              (the default verb — the bare curl-pipe is a complete install)
      --force move an existing foreign `cargo` aside to cargo.pre-gantry.bak
  uninstall   reverse the install (delegates to `gantry uninstall`)
  doctor      health-check the install (delegates to `gantry doctor`)

environment: GANTRY_INSTALL_FORGE, GANTRY_INSTALL_REPO, GANTRY_INSTALL_VERSION,
  GANTRY_BIN_DIR, GANTRY_INSTALL_LOCAL_BIN (see the script header)
EOF
}

# --- fetch helpers ----------------------------------------------------------
# curl or wget, whichever the box has; a fresh box is allowed to have either,
# and the failure when it has neither must name the fix, not scroll a trace.

fetch() {
  _url="$1"; _dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$_url" -o "$_dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$_dest" "$_url"
  else
    err "need curl or wget to download — install one and retry"
    return 127
  fi
}

sha256_of() {
  # 256-bit hex digest of $1, via coreutils sha256sum or BSD shasum.
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    err "need sha256sum or shasum to verify the download"
    return 127
  fi
}

latest_tag() {
  # The forge's latest release tag, pulled from the JSON with sed — a fresh
  # box is not assumed to carry jq or python. Forgejo spells the field
  # `"tag_name": "v1.2.3"` (the space after the colon varies between
  # releases), so both spellings are consumed; the first match wins.
  fetch "$FORGE/api/v1/repos/$REPO/releases/latest" "$tmpdir/latest.json" || return 1
  sed -n 's/.*"tag_name"[: ]*"\([^"]*\)".*/\1/p' "$tmpdir/latest.json" | head -n 1
}

# --- install ----------------------------------------------------------------

install_binary() {
  # Copy (never move — the source may be the operator's staged file), then
  # rename over any previous binary: a concurrent `cargo` sees the old or
  # the new complete file, never a half-written one.
  _src="$1"
  mkdir -p "$BIN_DIR" || { err "cannot create $BIN_DIR"; return 1; }
  cp "$_src" "$BIN_DIR/gantry.new" || return 1
  chmod 755 "$BIN_DIR/gantry.new" || return 1
  mv -f "$BIN_DIR/gantry.new" "$BIN_DIR/gantry" || return 1
  info "binary installed: $BIN_DIR/gantry"
}

install_shim() {
  # The shim rides beside the binary under its well-known name; the guard
  # rails below all speak of this path.
  _link="$BIN_DIR/cargo"

  # Already gantry's by the symlink spelling? Recreate it so it points at
  # the just-installed binary, whatever it pointed at before.
  if [ -L "$_link" ]; then
    _target=$(readlink "$_link" 2>/dev/null || true)
    case "$_target" in
      */gantry|gantry)
        rm -f "$_link"
        ln -s gantry "$_link"
        info "shim updated: $_link -> gantry"
        return 0
        ;;
    esac
  fi

  # The copy spelling of the same rule: a regular file byte-identical to the
  # installed binary is gantry's own shim (uninstall.rs accepts both), so it
  # is replaced in place — with the symlink, the layout this installer lays.
  if [ -f "$_link" ] && [ ! -L "$_link" ]; then
    if cmp -s "$_link" "$BIN_DIR/gantry"; then
      rm -f "$_link"
      ln -s gantry "$_link"
      info "shim updated (was a gantry copy): $_link -> gantry"
      return 0
    fi
  fi

  # A dangling symlink shadows cargo while pointing at nothing — dead weight
  # from some earlier install, safe to replace.
  if [ -L "$_link" ] && [ ! -e "$_link" ]; then
    rm -f "$_link"
  fi

  # Anything still standing is foreign — someone else's toolchain, exactly
  # what uninstall refuses to delete and install refuses to overwrite. Only
  # --force moves it aside, and even then it is kept, not deleted.
  if [ -e "$_link" ]; then
    if [ "$FORCE" != "1" ]; then
      err "refusing to replace non-gantry '$_link'"
      err "rerun with --force to move it aside to cargo.pre-gantry.bak"
      return 1
    fi
    mv -f "$_link" "$BIN_DIR/cargo.pre-gantry.bak"
    info "moved foreign shim aside: $_link -> cargo.pre-gantry.bak"
  fi

  ln -s gantry "$_link" || { err "cannot create shim at $_link"; return 1; }
  info "shim installed: $_link -> gantry"
}

check_path() {
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
      info "$BIN_DIR is not on PATH — add it, e.g.:"
      info "  export PATH=\"$BIN_DIR:\$PATH\""
      ;;
  esac
}

# The shim guard, run BEFORE anything is laid: a refused install must leave
# the box exactly as it found it (no binary, no partial state), so the
# foreign-cargo check happens first and install_shim — which re-checks with
# the new binary in place — can only ever see a clear field.
shim_preflight() {
  _link="$BIN_DIR/cargo"
  [ -e "$_link" ] || return 0
  # Gantry's own spellings are never foreign: the symlink this installer
  # lays, and a byte-identical copy of the installed binary.
  if [ -L "$_link" ]; then
    return 0
  fi
  if [ -f "$_link" ] && cmp -s "$_link" "$BIN_DIR/gantry" 2>/dev/null; then
    return 0
  fi
  [ "$FORCE" = "1" ] && return 0
  err "refusing to replace non-gantry '$_link'"
  err "rerun with --force to move it aside to cargo.pre-gantry.bak"
  return 1
}

verb_install() {
  # --force is the only install-scoped flag; anything else is usage.
  while [ $# -gt 0 ]; do
    case "$1" in
      --force) FORCE=1 ;;
      *) usage; return 2 ;;
    esac
    shift
  done

  # Refuse before anything is laid or fetched — a refused install leaves no
  # trace (tests/install_script_integration.rs pins this).
  shim_preflight || return 1

  if [ -n "${GANTRY_INSTALL_LOCAL_BIN:-}" ]; then
    [ -f "$GANTRY_INSTALL_LOCAL_BIN" ] || { err "GANTRY_INSTALL_LOCAL_BIN=$GANTRY_INSTALL_LOCAL_BIN is not a file"; return 2; }
    info "installing from local file: $GANTRY_INSTALL_LOCAL_BIN"
    install_binary "$GANTRY_INSTALL_LOCAL_BIN" || return 1
  else
    tmpdir=$(mktemp -d) || { err "cannot create a temp dir"; return 1; }
    trap 'rm -rf "$tmpdir"' EXIT INT TERM

    if [ -n "${GANTRY_INSTALL_VERSION:-}" ]; then
      tag="$GANTRY_INSTALL_VERSION"
    else
      tag=$(latest_tag) || { err "cannot determine the latest release tag — pass GANTRY_INSTALL_VERSION"; return 1; }
      [ -n "$tag" ] || { err "cannot parse the latest release tag — pass GANTRY_INSTALL_VERSION"; return 1; }
    fi

    case "$(uname -m)" in
      x86_64) TRIPLET="x86_64" ;;
      aarch64|arm64) TRIPLET="aarch64" ;;
      *) err "unsupported architecture: $(uname -m) (releases ship x86_64 today)"; return 1 ;;
    esac
    tarball="gantry-$TRIPLET-linux-musl.tar.gz"
    base="$FORGE/$REPO/releases/download/$tag"

    info "downloading $base/$tarball"
    fetch "$base/$tarball" "$tmpdir/$tarball" || { err "download failed: $base/$tarball"; return 1; }
    fetch "$base/checksums.txt" "$tmpdir/checksums.txt" || { err "download failed: $base/checksums.txt"; return 1; }

    want=$(grep "  $tarball\$" "$tmpdir/checksums.txt" | cut -d' ' -f1 | head -n 1)
    [ -n "$want" ] || { err "$tarball not listed in checksums.txt"; return 1; }
    got=$(sha256_of "$tmpdir/$tarball") || return 1
    [ "$got" = "$want" ] || { err "checksum mismatch for $tarball: want $want got $got"; return 1; }
    info "checksum ok"

    tar -xzf "$tmpdir/$tarball" -C "$tmpdir" || { err "cannot unpack $tarball"; return 1; }
    [ -f "$tmpdir/gantry" ] || { err "$tarball does not contain a gantry binary"; return 1; }
    install_binary "$tmpdir/gantry" || return 1
  fi

  install_shim || return 1
  check_path

  # The acceptance gate: a fresh install must reach a passing quickcheck with
  # zero config. Run the binary directly — quickcheck is the Tier-0 proof and
  # needs no shim spelling to be meaningful.
  if "$BIN_DIR/gantry" quickcheck; then
    info "quickcheck passed — gantry is ready (zero config, Tier-0)"
  else
    err "quickcheck FAILED — see the [gantry] quickcheck lines above"
    return 1
  fi
}

# --- uninstall / doctor -----------------------------------------------------
# Thin delegates: the binary owns both flows (uninstall.rs reverses the whole
# layout; doctor computes and prints the checks). The script locates the
# binary and relays the exit code — nothing else, so the verbs can never
# drift from what the binary actually does. A leading `--` separates the
# script's own argument space from the binary's; it is optional.

delegate() {
  _verb="$1"; shift
  [ -x "$BIN_DIR/gantry" ] || { err "no gantry binary at $BIN_DIR/gantry — nothing to $_verb"; return 1; }
  exec "$BIN_DIR/gantry" "$_verb" "$@"
}

verb_uninstall() {
  if [ "$#" -gt 0 ] && [ "$1" = "--" ]; then shift; fi
  delegate uninstall "$@"
}

verb_doctor() {
  if [ "$#" -gt 0 ] && [ "$1" = "--" ]; then shift; fi
  delegate doctor "$@"
}

# --- dispatch ---------------------------------------------------------------

main() {
  verb="${1:-install}"
  # Shift the verb off the argument list before handing the rest to the verb.
  # The guard matters for the bare curl-pipe (`| sh`): `shift` with no
  # positionals is shell-defined behavior, and the install must not depend on
  # it. (The shift happens here, in main — a `shift` inside a *function*
  # shifts that function's own positionals, not the caller's.)
  if [ "$#" -gt 0 ]; then shift; fi
  case "$verb" in
    install)    verb_install "$@" ;;
    uninstall)  verb_uninstall "$@" ;;
    doctor)     verb_doctor "$@" ;;
    -h|--help|help) usage; return 0 ;;
    *) usage; return 2 ;;
  esac
}

main "$@"
