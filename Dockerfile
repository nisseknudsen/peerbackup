# Client image: peerbackup plus restic, statically linked, no runtime deps.
#
# Read the Docker section of README.md before using this. Source directories must be mounted at
# the same paths they have on the host, because those paths are recorded in the
# backup and appear in your recovery instructions. peerbackup refuses to run if
# a configured source is missing, so a forgotten mount fails immediately rather
# than producing a backup without your data in it.

# Supported platforms: linux/amd64 and linux/arm64.
#
# Needs BuildKit, which has been Docker's default builder since 23.0. The legacy
# builder (DOCKER_BUILDKIT=0) does not set $BUILDPLATFORM and stops at the first
# line with a platform parse error -- loudly, before anything is built.
#
# The build stage runs on the machine doing the build ($BUILDPLATFORM) and
# cross-compiles to the requested one ($TARGETARCH). Compiling Rust under QEMU
# for an arm64 image took long enough to be the slowest thing in the pipeline,
# and it is unnecessary: the binary is static musl, so the only cross tool it
# needs is a linker, and the toolchain already ships one in rust-lld. No cross
# gcc, no extra packages.
#
# Pinned by digest, not by tag, for the same reason restic is pinned by
# checksum: `rust:1.88-alpine` is a moving target that upstream can repoint
# under you. These are manifest-list digests, so multi-architecture builds still
# resolve to the right image. To bump one:
#   docker buildx imagetools inspect rust:1.88-alpine
FROM --platform=$BUILDPLATFORM rust:1.88-alpine@sha256:9dfaae478ecd298b6b5a039e1f2cc4fc040fc818a2de9aa78fa714dea036574d AS build
ARG TARGETARCH
RUN apk add --no-cache musl-dev
# The Rust target for the requested platform, written to a file so the later
# RUN steps agree on it.
#
# TARGETARCH is required rather than defaulted from `uname -m`. This stage runs
# on the build platform, so uname reports the build machine -- a fallback here
# would compile an amd64 binary for an arm64 image and say nothing.
#
# rust-lld is the linker only when cross-compiling. This image's host triple is
# itself a musl one, and cargo applies a target's linker setting to host builds
# too when the two triples match -- so setting it unconditionally also linked
# the proc-macro crates (serde_derive, clap_derive) with rust-lld, and those are
# dynamic libraries that need the system linker. A native build keeps cc.
RUN set -eu; \
    case "${TARGETARCH:-}" in \
      amd64) t=x86_64-unknown-linux-musl ;; \
      arm64) t=aarch64-unknown-linux-musl ;; \
      "")    echo "TARGETARCH is not set: build with BuildKit (docker buildx build)" >&2; exit 1 ;; \
      *)     echo "unsupported architecture '$TARGETARCH': peerbackup builds for amd64 and arm64" >&2; exit 1 ;; \
    esac; \
    host="$(rustc -vV | sed -n 's/^host: //p')"; \
    if [ "$t" = "$host" ]; then \
      : > /rust-env; \
    else \
      rustup target add "$t"; \
      echo "export CARGO_TARGET_$(echo "$t" | tr 'a-z-' 'A-Z_')_LINKER=rust-lld" > /rust-env; \
    fi; \
    echo "$t" > /rust-target
WORKDIR /src
# Dependencies first, against a stub main, so editing src does not rebuild the
# whole tree. Cargo.lock is copied with it, so the cached layer is invalidated
# by a dependency change and by nothing else.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
 && . /rust-env && cargo build --release --locked --target "$(cat /rust-target)" \
 && rm -rf src
COPY src ./src
# Cargo decides on mtime, and COPY can preserve one older than the stub build.
RUN touch src/main.rs \
 && . /rust-env && cargo build --release --locked --target "$(cat /rust-target)" \
 && install -D -m 0755 "target/$(cat /rust-target)/release/peerbackup" /out/peerbackup

# The binary alone. The release workflow exports this stage with
# `--target binary --output type=local` and publishes what comes out, then
# builds the image from the same builder. The build stage is cached between the
# two, so the binary on the releases page and the binary in the image are the
# same bytes -- and the workflow checks that they are before tagging anything.
FROM scratch AS binary
COPY --from=build /out/peerbackup /peerbackup

# restic stays pinned by checksum, per architecture. Both values below were
# checked against restic's own SHA256SUMS for the release, and that file's
# signature against restic's release key (CF8F18F2844575973F79D4E191A6868BD3F7A907).
# A build for an architecture with no pinned digest fails loudly rather than
# falling back to an unverified download, which would quietly drop the property
# this pinning exists for.
#
# Runs on the build platform too: it only downloads and verifies a file for the
# target architecture, and never executes it, so there is nothing to emulate.
FROM --platform=$BUILDPLATFORM alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc AS restic
# Required, never defaulted. A default of amd64 once meant an arm64 build fetched
# the amd64 restic, verified it against its own correct checksum, and installed
# it next to an arm64 peerbackup -- which surfaced as `exec format error` at
# backup time rather than at build time. Falling back to `uname -m` would now be
# worse still: this stage runs on the build platform, so uname names the build
# machine, not the image being built.
ARG TARGETARCH
ARG RESTIC_VERSION=0.19.1
ARG RESTIC_SHA256_amd64=f415415624dcc452f2a02b8c33641791a8c6d6d3b65bbb3543fcf9a25151585c
ARG RESTIC_SHA256_arm64=a5f64aaab53d51e311fa3829124c5b703f2d14cf187d8640b6be3b2b49376465
RUN set -eu; \
    arch="${TARGETARCH:-}"; \
    if [ -z "$arch" ]; then \
      echo "TARGETARCH is not set: build with BuildKit (docker buildx build)" >&2; \
      exit 1; \
    fi; \
    case "$arch" in \
      amd64) sha="$RESTIC_SHA256_amd64" ;; \
      arm64) sha="$RESTIC_SHA256_arm64" ;; \
      *)     sha="" ;; \
    esac; \
    if [ -z "$sha" ]; then \
      echo "no pinned restic checksum for architecture '$arch'." >&2; \
      echo "Add RESTIC_SHA256_$arch to the Dockerfile from the release SHA256SUMS," >&2; \
      echo "or pass it: docker build --build-arg RESTIC_SHA256_$arch=<digest> ." >&2; \
      exit 1; \
    fi; \
    apk add --no-cache curl bzip2; \
    f="restic_${RESTIC_VERSION}_linux_${arch}"; \
    curl -sSLO "https://github.com/restic/restic/releases/download/v${RESTIC_VERSION}/${f}.bz2"; \
    echo "${sha}  ${f}.bz2" | sha256sum -c -; \
    bunzip2 "${f}.bz2"; \
    install -m 0755 "$f" /usr/local/bin/restic

FROM alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc
# tini, because peerbackup runs as PID 1 here and PID 1 has no default signal
# dispositions: a signal with no handler installed is ignored rather than
# terminating the process. So `docker stop` and `systemctl stop` hung for the
# full ten-second grace period and then SIGKILLed, which kills peerbackup
# without killing the restic it spawned -- and an abandoned restic leaves its
# repository lock behind for the next run to trip over.
#
# `-g` signals the whole process group, so restic gets the SIGTERM too and
# removes its own lock on the way out. `docker run --init` does the same thing
# and is what this replaces; baking it in means nobody has to know that.
RUN apk add --no-cache ca-certificates tini
COPY --from=restic /usr/local/bin/restic /usr/local/bin/restic
COPY --from=binary /peerbackup /usr/local/bin/peerbackup

# Config and recorded results live here. Both must be mounted, or every run
# starts from nothing and status has no history to report.
#
# HOME and XDG_CACHE_HOME point into /state deliberately. restic keeps a cache
# and locates it from HOME; with `--user` set to an id that has no entry in
# /etc/passwd, HOME resolves to / and restic fails with
# "mkdir /.cache: permission denied". Since people are told to pass their own
# uid, that is the normal case, not an edge case. Putting the cache in a mounted
# volume also means it survives between runs, which restic uses to avoid
# re-reading unchanged data.
ENV PEERBACKUP_CONFIG_DIR=/config \
    PEERBACKUP_STATE_DIR=/state \
    HOME=/state \
    XDG_CACHE_HOME=/state/cache

# A default for people who do not pass --user. Any uid works: nothing depends on
# this account existing, because every writable path is an absolute one in a
# mounted volume.
RUN adduser -D -u 1000 peerbackup

# Created in the image, before VOLUME, and owned by that uid. Declaring a volume
# on a path the image does not have means Docker materialises it as root-owned,
# so forgetting `-v` gave an unprivileged container a permission error rather
# than a message naming the mount it was missing.
RUN mkdir -p /config /state /state/cache && chown -R 1000:1000 /config /state
VOLUME ["/config", "/state"]

USER peerbackup

ENTRYPOINT ["/sbin/tini", "-g", "--", "peerbackup"]
CMD ["status"]
