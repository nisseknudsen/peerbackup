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
ENV PEERBACKUP_CONFIG_DIR=/config \
    PEERBACKUP_STATE_DIR=/state
VOLUME ["/config", "/state"]

# Runs as an ordinary user by default. Override with `--user` to match the
# ownership of the files you are backing up.
RUN adduser -D -u 1000 peerbackup
USER peerbackup

ENTRYPOINT ["peerbackup"]
CMD ["status"]
