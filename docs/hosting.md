# Hosting

How to run the receiving side for the long term. `peerbackup host quickstart`
is enough to get started (see the [README](../README.md#quick-start)); this page
covers everything after that.

- [Quickstart or compose](#quickstart-or-compose)
- [Reverse proxy](#reverse-proxy)
- [TLS](#tls)
- [A size limit per peer](#a-size-limit-per-peer)
- [Running it as a service](#running-it-as-a-service)
- [Maintenance windows](#maintenance-windows)
- [Settings](#settings)

## What the server enforces

The server is [rest-server](https://github.com/restic/rest-server), started
with:

- `--private-repos`: each login can only reach its own repository, so peers
  cannot see each other's backups;
- `--append-only`: new backups are accepted, deletion is refused, so a
  compromised client cannot erase its own backup history (see
  [Maintenance windows](#maintenance-windows));
- `--max-size`: a total size limit across all peers (500 GiB by default, set
  with `PB_MAX_SIZE`).

Backups arrive encrypted. As the host you can see how much each peer stores, but
not what.

## Quickstart or compose

`host quickstart` and the shipped [`compose.yml`](../compose.yml) start the same
server, in a container named `peerbackup-rest`. They are two ways of running one
server, not two servers. To switch from one to the other, remove the container
first:

```sh
docker rm -f peerbackup-rest
```

Use compose when you need TLS in rest-server itself (`PB_EXTRA_OPTIONS`, which
`quickstart` cannot pass), or want the configuration in a file:

```sh
PB_DATA=~/.local/share/peerbackup-data docker compose up -d
peerbackup host adduser alice
```

Always add logins with `host adduser` rather than rest-server's own
`create_user`. rest-server reads its password file only at startup, so a login
added without a restart is rejected with `401`; `adduser` restarts the server
and checks the new login before returning.

## Reverse proxy

If the machine already runs Traefik, Caddy or nginx with a certificate for your
other services, use it for peerbackup too, and skip [TLS](#tls) below.

- Point the proxy at rest-server's plain HTTP port: `peerbackup-rest:8000` if the
  proxy is on the same Docker network, otherwise the published port.
- Do not publish port 51515 to the internet. Only the proxy's 80 and 443 need to
  be reachable. Set `PB_BIND=127.0.0.1` to keep the published port off other
  interfaces.
- The invite URL then has no port: `rest:https://alice@your-domain.example/alice/`.

### Turn off HTTP/2 for the peerbackup hostname

This setting decides how fast peers can upload to you. HTTP/2 carries all of a
client's parallel uploads on one TCP connection, which is slow over long
distances regardless of bandwidth: about 55 Mbit/s between two gigabit lines
170 ms apart, against roughly four times that over HTTP/1.1.

The protocol is agreed during the TLS handshake, so whoever holds the
certificate decides it. restic cannot opt out from its side.

| Setup | Who decides | What to change |
|---|---|---|
| A reverse proxy holds the certificate | The proxy | The proxy, as below |
| rest-server runs `--tls` itself | rest-server | Nothing: `compose.yml` already sets `GODEBUG=http2server=0` |
| Plain HTTP, no TLS | Nobody | Nothing: HTTP/2 requires TLS here |

**Traefik** can do this per hostname, so other sites keep HTTP/2. TLS options
cannot be set from Docker labels, so declare one in the dynamic file
configuration:

```yaml
# dynamic configuration file
tls:
  options:
    http1only:
      alpnProtocols:
        - http/1.1
        - acme-tls/1   # keep if you use the TLS-ALPN certificate challenge
```

and attach it to the peerbackup router:

```yaml
- traefik.http.routers.peerbackup.tls.options=http1only@file
```

Give peerbackup its own hostname. If two routers on the same hostname use
different TLS options, Traefik falls back to its defaults for both, which
re-enables HTTP/2.

**Caddy** sets protocols per listening port, for every site on that port:

```
{
	servers :443 {
		protocols h1
	}
}
```

To keep HTTP/2 for other sites, serve peerbackup on a separate port with its own
`servers :8443 { protocols h1 }` block, and use that port in the invite URL.

**nginx** only uses HTTP/2 where configured: remove `http2 on;` from the
peerbackup `server` block (or `http2` from its `listen` line on older versions).

**Check** after reloading the proxy. This should print `http/1.1`:

```sh
echo | openssl s_client -connect your-domain.example:443 -alpn h2,http/1.1 2>/dev/null \
  | grep 'ALPN protocol'
```

### Health checks

`compose.yml`'s health check passes on any HTTP response, not a specific status
code. With `--private-repos`, rest-server answers `/` with `401`, and the
container's `wget` treats that the same as no answer, so a check for a specific
code would mark a working server unhealthy. Keep this in mind if your proxy
routes based on container health.

## TLS

For rest-server holding the certificate itself. If a reverse proxy already does
TLS for you, use [Reverse proxy](#reverse-proxy) instead.

The container runs as `PB_UID`, not root, so it needs its own readable copy of
the certificate. Let's Encrypt's live directory is root-only, so copy the files
with `sudo` and hand them to your user:

```sh
export PB_CERTS=~/.local/share/peerbackup-certs
mkdir -p "$PB_CERTS"
sudo cp /etc/letsencrypt/live/example.org/fullchain.pem "$PB_CERTS/"
sudo cp /etc/letsencrypt/live/example.org/privkey.pem   "$PB_CERTS/"
sudo chown "$(id -u):$(id -g)" "$PB_CERTS"/*.pem
chmod 600 "$PB_CERTS/privkey.pem"
```

Keep the key at 0600. The container can read it because it runs as you.

Uncomment the certs volume in `compose.yml`, then start it with TLS enabled:

```sh
PB_CERTS=~/.local/share/peerbackup-certs \
PB_EXTRA_OPTIONS="--tls --tls-cert /certs/fullchain.pem --tls-key /certs/privkey.pem" \
PB_HEALTHCHECK_SCHEME=https \
  docker compose up -d
```

`PB_HEALTHCHECK_SCHEME=https` matters: without it the health check sends plain
HTTP to the TLS port, gets a `400` back, and reports healthy even if the
certificate failed to load.

Renewal replaces the files under `/etc/letsencrypt`, not your copies. Repeat the
copy and run `docker compose restart` when the certificate is renewed.

Peers then use `rest:https://...`. If the certificate is self-signed, they need
a copy and pass it with `--cacert` when connecting; see
[configuration.md](configuration.md#self-signed-certificates).

## A size limit per peer

`quickstart` gives the whole server one limit shared by all peers, so one peer
can fill it. To give each peer its own limit, enforced by the kernel:

```sh
sudo peerbackup host provision alice 500G
sudo peerbackup host adduser alice
sudo peerbackup host list
```

`provision` creates a fixed-size disk image for the peer, formats it, and mounts
it through a systemd mount unit under `/srv/peerbackup/mnt/alice`. When a peer
fills their image, their backups fail with `507 Insufficient Storage`, and no
one else is affected. It needs root because mounting does.

The image is fully allocated up front. `provision` refuses a filesystem where
that allocation would not actually reserve disk space (overlayfs, tmpfs, some
network filesystems), and refuses a grant that would leave less than
`HOST_MARGIN_GB` (default 20) free on the host.

```
$ sudo peerbackup host list
PEER                  IMAGE       USABLE      RESERVE         USED  STATE
alice                  500GB        491GB         73GB        112GB  mounted
```

- `USABLE` is smaller than `IMAGE` because of filesystem overhead.
- `RESERVE` is the headroom a peer's cleanup (`restic prune`) needs to repack
  data, `MAINTENANCE_RESERVE_PCT` percent (default 15) of the usable space.
  Nothing enforces it; it is shown so you can leave room. If `USED` goes past
  `USABLE` minus `RESERVE`, cleanup may fail.

To give the space back, which **destroys that peer's backups**:

```sh
sudo peerbackup host release alice
```

It asks you to type the peer name. `--force` skips the prompt, for scripts.

Every `host` command takes `--dry-run` to print what it would do without doing
it. Use the flag, not `DRY_RUN=1`: `sudo` clears the environment, so
`DRY_RUN=1 sudo peerbackup host provision ...` performs a real provision.

## Running it as a service

```sh
sudo install -m 0755 peerbackup /usr/local/bin/
sudo mkdir -p /usr/local/share/peerbackup
sudo cp compose.yml /usr/local/share/peerbackup/
sudo cp deploy/systemd/peerbackup-rest.service /etc/systemd/system/
```

Edit `/etc/systemd/system/peerbackup-rest.service` before starting it:

- set `PB_UID` and `PB_GID` to your own user;
- `PB_DATA=/srv/peerbackup/mnt` assumes per-peer images. If you used
  `quickstart`, set it to your data directory (`~/.local/share/peerbackup-data`
  by default), or the service will serve an empty directory.

Then:

```sh
sudo systemd-analyze verify /etc/systemd/system/peerbackup-rest.service
sudo systemctl daemon-reload
sudo systemctl enable --now peerbackup-rest
systemctl status peerbackup-rest
```

The unit runs as root, because it needs to read mount state and use Docker's
socket, and is sandboxed to those. Its filesystem restrictions fail closed: if
you change `PEERBACKUP_ROOT` or move the compose file, add the new path to
`ReadWritePaths` or the service will not start.

Before starting the container, the unit runs `peerbackup host guard`. With
per-peer images, `guard` refuses to start the server if any image is not
mounted; otherwise a boot where Docker starts before the mounts would write
peers' data to your root filesystem with no size limit. To confirm it works,
mask one mount unit and reboot: the service should fail to start. Without
per-peer images, `guard` finds nothing to check and lets the service start.

## Maintenance windows

Deletion is refused by default, so when a peer wants to remove old backups, you
open a short window without `--append-only`:

```sh
docker compose down
docker run --rm -d --name peerbackup-maint \
  --user "$(id -u):$(id -g)" -p 51515:8000 \
  -v "${PB_DATA:-$HOME/.local/share/peerbackup-data}:/data" \
  -e OPTIONS="--private-repos --max-size ${PB_MAX_SIZE:-536870912000}" \
  -e GODEBUG=http2server=0 \
  restic/rest-server:0.14.0
# the peer runs their cleanup, then:
docker rm -f peerbackup-maint
docker compose up -d
```

Use the same storage directory as the real server: with per-peer images that is
`/srv/peerbackup/mnt`, not the default above. Pointing it elsewhere gives the
peer an empty server on the right port.

Deletion is possible for every peer during the window, so keep it short.

## Settings

`compose.yml` reads these environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `PB_DATA` | `~/.local/share/peerbackup-data` | Where backups are stored |
| `PB_PORT` | `51515` | Published port |
| `PB_BIND` | `0.0.0.0` | Address the published port listens on |
| `PB_UID` / `PB_GID` | `1000` | User and group the server runs as, and that owns the stored files |
| `PB_MAX_SIZE` | `536870912000` | Total size limit for all peers, in bytes |
| `PB_EXTRA_OPTIONS` | empty | Extra rest-server flags, e.g. for TLS |
| `PB_CERTS` | `./certs` | Directory with `fullchain.pem` and `privkey.pem` |
| `PB_HEALTHCHECK_SCHEME` | `http` | Set to `https` when rest-server does TLS |

The `host` commands also read:

| Variable | Default | Meaning |
|---|---|---|
| `PB_DATA`, `PB_PORT`, `PB_UID`, `PB_GID` | as above | Used by `quickstart` and `adduser` |
| `PB_MAX_SIZE` | `536870912000` | Size limit `quickstart` starts the server with; accepts `500G` |
| `PB_CONTAINER` | `peerbackup-rest` | Container `quickstart` and `adduser` act on |
| `PB_COMPOSE_FILE` | the shipped `compose.yml` | Compose file used by `host up` / `host down` |
| `PEERBACKUP_ROOT` | `/srv/peerbackup` | Where `provision` keeps images (`images/`) and mount points (`mnt/`) |
| `SYSTEMD_UNIT_DIR` | `/etc/systemd/system` | Where `provision` writes mount units |
| `HOST_MARGIN_GB` | `20` | Free space `provision` always leaves on the host |
| `MAINTENANCE_RESERVE_PCT` | `15` | Reserve shown by `host list` |
| `DRY_RUN` | unset | `1` behaves like `--dry-run` |
| `PB_FORCE` | unset | `1` behaves like `--force` |
| `QUIET` | unset | `1` hides progress messages. A generated password is always shown. |
