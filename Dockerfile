# Client image: peerbackup plus restic, statically linked, no runtime deps.
#
# Read the Docker section of README.md before using this. Source directories must be mounted at
# the same paths they have on the host, because those paths are recorded in the
# backup and appear in your recovery instructions. peerbackup refuses to run if
# a configured source is missing, so a forgotten mount fails immediately rather
# than producing a backup without your data in it.

# No `--target`: the rust:alpine images are already musl-hosted, so the default
# target is the static one on whatever architecture is building. Naming
# x86_64-unknown-linux-musl outright meant the image could not be built for
# arm64 at all, which rules out a fair share of the homeservers this is for.
FROM rust:1.88-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Dependencies first, against a stub main, so editing src does not rebuild the
# whole tree. Cargo.lock is copied with it, so the cached layer is invalidated
# by a dependency change and by nothing else.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release \
 && rm -rf src
COPY src ./src
# Cargo decides on mtime, and COPY can preserve one older than the stub build.
RUN touch src/main.rs && cargo build --release

# restic stays pinned by checksum, per architecture. Only the checksum actually
# verified upstream is recorded here: a build for an architecture with no pinned
# digest fails loudly rather than falling back to an unverified download, which
# would quietly drop the property this pinning exists for. To add one, take the
# value from the release's own SHA256SUMS and put it below.
FROM alpine:3.20 AS restic
ARG TARGETARCH=amd64
ARG RESTIC_VERSION=0.19.1
ARG RESTIC_SHA256_amd64=f415415624dcc452f2a02b8c33641791a8c6d6d3b65bbb3543fcf9a25151585c
ARG RESTIC_SHA256_arm64=
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) sha="$RESTIC_SHA256_amd64" ;; \
      arm64) sha="$RESTIC_SHA256_arm64" ;; \
      *)     sha="" ;; \
    esac; \
    if [ -z "$sha" ]; then \
      echo "no pinned restic checksum for TARGETARCH=$TARGETARCH." >&2; \
      echo "Add RESTIC_SHA256_$TARGETARCH to the Dockerfile from the release SHA256SUMS," >&2; \
      echo "or pass it: docker build --build-arg RESTIC_SHA256_$TARGETARCH=<digest> ." >&2; \
      exit 1; \
    fi; \
    apk add --no-cache curl bzip2; \
    f="restic_${RESTIC_VERSION}_linux_${TARGETARCH}"; \
    curl -sSLO "https://github.com/restic/restic/releases/download/v${RESTIC_VERSION}/${f}.bz2"; \
    echo "${sha}  ${f}.bz2" | sha256sum -c -; \
    bunzip2 "${f}.bz2"; \
    install -m 0755 "$f" /usr/local/bin/restic

FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY --from=restic /usr/local/bin/restic /usr/local/bin/restic
COPY --from=build /src/target/release/peerbackup /usr/local/bin/peerbackup

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
VOLUME ["/config", "/state"]

# A default for people who do not pass --user. Any uid works: nothing depends on
# this account existing, because every writable path is an absolute one in a
# mounted volume.
RUN adduser -D -u 1000 peerbackup
USER peerbackup

ENTRYPOINT ["peerbackup"]
CMD ["status"]
