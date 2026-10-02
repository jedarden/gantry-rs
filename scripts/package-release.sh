#!/bin/sh
# gantry release packaging — static musl tarballs + checksums (plan Phase 3:
# "musl static build"; bead bf-3qm). The single packaging definition shared by
# `gantry-ci` (declarative-config k8s/iad-ci/argo-workflows/gantry-ci-
# workflowtemplate.yml) and any local release rehearsal — the CI template
# calls this script rather than carrying its own copy, so the tarball layout
# the installer consumes has exactly one author.
#
#   scripts/package-release.sh [--target <triplet>]... [--out <dir>]
#
# Produces, in --out (default dist/):
#   gantry-<arch>-linux-musl.tar.gz   one per target — a gzipped tar whose
#                                     single member is the `gantry` binary,
#                                     the exact layout install.sh unpacks
#   checksums.txt                     one `sha256sum` line per artifact
#                                     (tarballs + install.sh) — the manifest
#                                     install.sh verifies the download
#                                     against before installing
#
# The <arch> token is the triplet's first component ("x86_64",
# "aarch64") — the same token install.sh derives from `uname -m`, so the two
# sides agree by construction.
#
# Why musl needs no C cross-toolchain: every entry in Cargo.toml is pure Rust
# (serde/serde_json/toml/dirs) or declares its libc externs without compiling
# C (libc), so rustc's self-contained musl linking (rust-lld plus the
# precompiled musl CRT rustup ships with the target's std) produces a fully
# static binary with the host's plain `cc` as the link driver — the
# configuration both this script and the CI container rely on.
#
# Run from the repository root (the manifest step requires ./install.sh).

set -eu

OUT=dist
TARGETS=""

usage() {
  cat >&2 <<'EOF'
usage: scripts/package-release.sh [--target <triplet>]... [--out <dir>]

  --target <triplet>  package this Rust target (repeatable); default is the
                      host arch's musl triplet (x86_64/aarch64)
  --out <dir>         output directory (default dist/)
EOF
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --target) [ $# -ge 2 ] || usage; TARGETS="$TARGETS $2"; shift 2 ;;
    --out)    [ $# -ge 2 ] || usage; OUT="$2"; shift 2 ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
done

# Host arch -> musl triplet, the default target set. Same uname mapping
# install.sh applies on the consuming side.
default_target() {
  case "$(uname -m)" in
    x86_64)          echo "x86_64-unknown-linux-musl" ;;
    aarch64|arm64)   echo "aarch64-unknown-linux-musl" ;;
    *) echo "package-release.sh: unsupported host arch: $(uname -m)" >&2; return 1 ;;
  esac
}

if [ -z "$TARGETS" ]; then
  TARGETS=" $(default_target)"
fi

[ -f install.sh ] || { echo "package-release.sh: run from the repository root (install.sh not found)" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "package-release.sh: cargo not on PATH" >&2; exit 1; }

mkdir -p "$OUT"

# sha256 of $1 as bare hex — coreutils sha256sum, BSD shasum fallback (the
# same dual support install.sh's verifier has, so the manifest writer can
# never produce a format its consumer cannot read).
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    echo "package-release.sh: need sha256sum or shasum to write checksums.txt" >&2
    return 127
  fi
}

for target in $TARGETS; do
  arch=${target%%-*}

  # Idempotent when the target std is already installed; fails loudly when
  # rustup cannot provide it (that is a real packaging failure, not a warning).
  rustup target add "$target" 1>&2

  cargo build --release --target "$target" 1>&2
  bin="target/$target/release/gantry"
  [ -f "$bin" ] || { echo "package-release.sh: no binary at $bin after the build" >&2; exit 1; }

  # Static proof (plan: "single static binary"). A dynamically linked musl
  # build would defeat the installer's whole premise — fail here, in the
  # packager, rather than ship a tarball that cannot run on a fresh box.
  if command -v readelf >/dev/null 2>&1; then
    if readelf -l "$bin" | grep -q INTERP; then
      echo "package-release.sh: $bin is dynamically linked (INTERP present) — refusing to package" >&2
      exit 1
    fi
  fi

  stage="$OUT/.pkg-$target"
  rm -rf "$stage"
  mkdir -p "$stage"
  cp "$bin" "$stage/gantry"
  chmod 755 "$stage/gantry"

  # gzip -n: no name/timestamp in the gzip header, so identical inputs give
  # byte-identical tarballs and the published checksums stay reproducible.
  ( cd "$stage" && tar -cf - gantry | gzip -n -9 ) > "$OUT/gantry-$arch-linux-musl.tar.gz"
  rm -rf "$stage"
  echo "packaged: $OUT/gantry-$arch-linux-musl.tar.gz"
done

# The manifest: every tarball this run produced, plus install.sh itself (it is
# attached to the release too, so a pinned-version install is checksummed the
# same way the tarballs are).
manifest="$OUT/checksums.txt"
: > "$manifest"
for f in "$OUT"/gantry-*-linux-musl.tar.gz; do
  [ -e "$f" ] || { echo "package-release.sh: no tarballs under $OUT" >&2; exit 1; }
  hash=$(sha256_of "$f")
  printf '%s  %s\n' "$hash" "$(basename "$f")" >> "$manifest"
done
hash=$(sha256_of install.sh)
printf '%s  %s\n' "$hash" "install.sh" >> "$manifest"

echo "manifest: $manifest"
cat "$manifest"
