# Client image: peerbackup plus restic, statically linked, no runtime deps.
#
# Read docker/README.md before using this. Source directories must be mounted at
# the same paths they have on the host, because those paths are recorded in the
# backup and appear in your recovery instructions. peerbackup refuses to run if
# a configured source is missing, so a forgotten mount fails immediately rather
# than producing a backup without your data in it.

FROM rust:1.88-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM alpine:3.20 AS restic
ARG RESTIC_VERSION=0.19.1
ARG RESTIC_SHA256=f415415624dcc452f2a02b8c33641791a8c6d6d3b65bbb3543fcf9a25151585c
RUN apk add --no-cache curl bzip2 \
 && curl -sSLO "https://github.com/restic/restic/releases/download/v${RESTIC_VERSION}/restic_${RESTIC_VERSION}_linux_amd64.bz2" \
 && echo "${RESTIC_SHA256}  restic_${RESTIC_VERSION}_linux_amd64.bz2" | sha256sum -c - \
 && bunzip2 "restic_${RESTIC_VERSION}_linux_amd64.bz2" \
 && install -m 0755 "restic_${RESTIC_VERSION}_linux_amd64" /usr/local/bin/restic

FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY --from=restic /usr/local/bin/restic /usr/local/bin/restic
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/peerbackup /usr/local/bin/peerbackup

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
