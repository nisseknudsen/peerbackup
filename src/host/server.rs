//! Running the receiving server, and creating the logins it serves.

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::{Ctx, DEFAULT_CONTAINER, DEFAULT_PORT, REST_SERVER_IMAGE, Res, have, warn};
use crate::config::{PeerName, random_token};

pub struct ServerOpts {
    pub port: u16,
    pub data: PathBuf,
    pub container: String,
    pub max_size: u64,
}

impl ServerOpts {
    /// Read the overrides, refusing any that cannot be understood.
    ///
    /// Falling back to a default when an operator has explicitly set something
    /// is the wrong shape here: PB_MAX_SIZE decides how much a friend can
    /// store, and silently substituting 500G for a typo means the limit you
    /// think you set is not the limit you have. Same argument as `parse_size`,
    /// which this now uses, so `PB_MAX_SIZE=500G` works as well as raw bytes.
    pub fn from_env() -> Result<Self, String> {
        let port = match std::env::var("PB_PORT") {
            Ok(v) => v
                .parse()
                .map_err(|_| format!("PB_PORT='{v}' is not a port number"))?,
            Err(_) => DEFAULT_PORT,
        };
        let max_size = match std::env::var("PB_MAX_SIZE") {
            Ok(v) => super::size::parse_size(&v).map_err(|e| format!("PB_MAX_SIZE: {e}"))?,
            Err(_) => 536_870_912_000,
        };
        Ok(Self {
            port,
            data: std::env::var_os("PB_DATA")
                .map(PathBuf::from)
                .unwrap_or_else(default_data_dir),
            container: std::env::var("PB_CONTAINER")
                .unwrap_or_else(|_| DEFAULT_CONTAINER.to_owned()),
            max_size,
        })
    }
}

/// Somewhere an ordinary user can write. `/srv` needs root, which defeats the
/// point of a setup that otherwise does not.
fn default_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::home().join(".local/share"))
        .join("peerbackup-data")
}

/// Start a server and hand back a URL, in one command and without root.
///
/// Uses a plain directory rather than a preallocated image, so the size limit
/// is rest-server's and is shared across all peers. `provision` is the version
/// with a per-peer limit the kernel enforces.
/// Does this container already hold a login for this peer?
///
/// `htpasswd -i` overwrites silently, and the credential it replaces is the one
/// the peer is actively using. Answering `false` when docker cannot be asked is
/// deliberate: the check exists to stop an accidental rotation, and refusing to
/// create a login because the *check* failed would be worse than the thing it
/// guards against.
fn has_login(container: &str, peer: &PeerName) -> bool {
    Command::new("docker")
        .args([
            "exec",
            container,
            "sh",
            "-c",
            r#"grep -q "^$1:" "$PASSWORD_FILE""#,
            "sh",
            peer.as_str(),
        ])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Where a running container's `/data` actually comes from on the host.
///
/// Returns `None` when docker cannot be asked or the container has no such
/// mount, because a missing answer is not evidence of a mismatch.
fn container_data_source(container: &str) -> Option<std::path::PathBuf> {
    let out = Command::new("docker")
        .args([
            "inspect",
            "-f",
            r#"{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}"#,
            container,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!s.is_empty()).then(|| std::path::PathBuf::from(s))
}

/// The uid and gid the server container should run as.
///
/// `getuid` is 0 under sudo, and `quickstart` does not need root -- so a habitual
/// `sudo peerbackup host quickstart alice` produced a server writing root-owned
/// files, which is the README's own troubleshooting entry. `SUDO_UID` is the
/// account that actually invoked it, and `PB_UID` overrides both so this can be
/// made to match the service unit.
fn server_ids() -> (u32, u32) {
    let from_env = |keys: [&str; 2]| {
        keys.iter()
            .find_map(|k| std::env::var(k).ok()?.trim().parse::<u32>().ok())
    };
    // SAFETY: getuid/getgid read process properties and cannot fail.
    let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
    (
        from_env(["PB_UID", "SUDO_UID"]).unwrap_or(u),
        from_env(["PB_GID", "SUDO_GID"]).unwrap_or(g),
    )
}

pub fn quickstart(ctx: &Ctx, peer: &PeerName, o: &ServerOpts) -> Res {
    if !have("docker") {
        return Err("docker is required".into());
    }
    if !docker_ok() {
        return Err(
            "cannot talk to docker; is the daemon running and are you in the docker group?".into(),
        );
    }

    if !ctx.would(&format!("mkdir -p {}", o.data.display())) {
        std::fs::create_dir_all(&o.data).map_err(|e| {
            format!(
                "cannot create {} ({e})\n       Pick somewhere you can write: \
                 PB_DATA=/path peerbackup host quickstart {peer}",
                o.data.display()
            )
        })?;
    }
    if !ctx.dry_run && !writable(&o.data) {
        return Err(format!(
            "{} exists but is not writable by this user\n       \
             Either: sudo chown -R $(id -un) '{}'\n       \
             Or pick another: PB_DATA=/path peerbackup host quickstart {peer}",
            o.data.display(),
            o.data.display()
        ));
    }

    if container_running(&o.container) {
        // Being up is not the same as serving on the port we are about to hand
        // out. A container left over from an earlier setup listens somewhere
        // else, and skipping this check is how a URL that cannot work gets
        // printed.
        if !http_reachable(o.port) {
            return Err(format!(
                "a container named '{}' is already running, but nothing answers\n       \
                 on port {}. It is probably left over from an earlier setup using a\n       \
                 different port or storage directory.\n\n       \
                 Look at it:  docker ps --filter name={}\n       \
                 Remove it:   docker rm -f {}\n       \
                 Then run this again.",
                o.container, o.port, o.container, o.container
            ));
        }
        ctx.info(&format!(
            "server already running on port {}, adding a peer to it",
            o.port
        ));
        // What it is actually serving, rather than what this invocation would
        // have told it to serve. The `Storage:` line printed at the end comes
        // from `o.data`, so adopting a container started against a different
        // directory reported a path with none of the peer's data in it -- and
        // on a host using per-peer grants, the wrong filesystem entirely.
        if let Some(actual) = container_data_source(&o.container)
            && actual != o.data
        {
            warn(&format!(
                "that container stores data in {}, not {}.\n       \
                 Everything below refers to the container's directory. If that is \
                 not what\n       you meant: docker rm -f {} and run this again.",
                actual.display(),
                o.data.display(),
                o.container
            ));
        }
    } else {
        // A dry run reached this and really deleted the container. `quickstart`
        // is the command someone rehearses precisely because they are not sure
        // what it will do, and `rm -f` on a stopped container takes its
        // configuration with it.
        if !ctx.would(&format!("docker rm -f {}", o.container)) {
            let _ = Command::new("docker")
                .args(["rm", "-f", &o.container])
                .output();
        }
        if !port_free(o.port) {
            return Err(format!(
                "port {} is already in use on this machine\n       \
                 Choose another: PB_PORT=51516 peerbackup host quickstart {peer}",
                o.port
            ));
        }

        ctx.info(&format!("starting the server on port {}", o.port));
        // Under sudo this is 0:0, and every file the server writes ends up
        // root-owned -- which is the README's own troubleshooting entry, "you
        // cannot read your own stored backups without sudo". `quickstart` does
        // not need root, so the fix is to use the account that invoked it.
        let (uid, gid) = server_ids();
        let user = format!("{uid}:{gid}");
        if uid == 0 {
            warn(
                "running the server as root. Everything it stores will be root-owned \
                 and you\n       will need sudo to read your own data. Set PB_UID and \
                 PB_GID, or run\n       this without sudo -- quickstart does not need it.",
            );
        }
        let run_args = server_run_args(o, &user);
        if ctx.would(&format!("docker {}", run_args.join(" "))) {
            ctx.info("");
            ctx.info(&format!(
                "would then create the login for '{peer}' and print an invite URL."
            ));
            return Ok(());
        }
        let out = Command::new("docker")
            .args(run_args)
            .output()
            .map_err(|e| format!("could not run docker: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "docker could not start the server. Full error:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        // Do not trust `docker run` exiting 0: the container can start and then
        // die a moment later.
        let mut up = false;
        for _ in 0..60 {
            if container_running(&o.container) && http_reachable(o.port) {
                up = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if !up {
            warn("the server did not come up. Its log:");
            if let Ok(logs) = Command::new("docker").args(["logs", &o.container]).output() {
                for line in String::from_utf8_lossy(&logs.stderr)
                    .lines()
                    .chain(String::from_utf8_lossy(&logs.stdout).lines())
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    eprintln!("  {line}");
                }
            }
            let _ = Command::new("docker")
                .args(["rm", "-f", &o.container])
                .output();
            return Err(format!("could not start the server on port {}", o.port));
        }
    }

    // Checked here as well as in `adduser`, so the message fits what the person
    // was actually doing. Re-running `quickstart` to see whether the server is
    // up is a reasonable thing to do, and it used to rotate the peer's password
    // as a side effect.
    if !ctx.force && has_login(&o.container, peer) {
        ctx.info("");
        ctx.info(&format!(
            "The server is running and '{peer}' already has a login on it."
        ));
        ctx.info("Nothing changed. peerbackup does not keep their password, so it");
        ctx.info("cannot print the invite again.");
        ctx.info("");
        ctx.info(&format!(
            "To issue a new password (their old one stops working):\n  \
             peerbackup host adduser {peer} --force"
        ));
        return Ok(());
    }

    let pw = adduser(ctx, peer, None, o).map_err(|e| {
        format!("server is running but the login for '{peer}' could not be created\n       {e}")
    })?;
    let pw = pw.ok_or_else(|| format!("no password was generated for '{peer}'"))?;

    let host = hostname();
    ctx.info("");
    // The address and the password, separately, because they are not the same
    // kind of thing. The address is not secret and belongs on a command line;
    // the password is and does not. `peerbackup connect` asks for it, so it
    // never reaches shell history, `ps`, or `docker inspect`.
    let url = format!("rest:http://{peer}@{host}:{}/{peer}/", o.port);
    ctx.info(&format!(
        "Ready. Send both of these to {peer}, over something you trust:"
    ));
    ctx.info("");
    ctx.info(&format!("  URL:      {url}"));
    ctx.info(&format!("  Password: {pw}"));
    ctx.info("");
    ctx.info("They run:");
    ctx.info(&format!(
        "  peerbackup connect '{url}' --source /path/to/back/up"
    ));
    ctx.info("");
    ctx.info("and paste the password when it asks.");
    ctx.info("");
    ctx.info(&format!("Storage:   {}", o.data.display()));
    ctx.info(&format!(
        "Port {} must reach this machine. Use TLS if it is exposed to the",
        o.port
    ));
    ctx.info("internet: see the README.");
    Ok(())
}

/// The `docker run` arguments for the rest-server container.
///
/// A function so the arguments can be asserted on without starting anything.
fn server_run_args(o: &ServerOpts, user: &str) -> Vec<String> {
    vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        o.container.clone(),
        "--restart".into(),
        "unless-stopped".into(),
        "--user".into(),
        user.to_owned(),
        "-p".into(),
        format!("{}:8000", o.port),
        "-v".into(),
        format!("{}:/data", o.data.display()),
        "-e".into(),
        format!(
            "OPTIONS=--private-repos --append-only --max-size {}",
            o.max_size
        ),
        "-e".into(),
        SERVE_HTTP1_ONLY.into(),
        REST_SERVER_IMAGE.into(),
    ]
}

/// Stop rest-server offering HTTP/2.
///
/// HTTP/2 multiplexes every parallel upload onto one TCP connection, and one
/// connection to a peer 170ms away carries about 55 Mbit/s however much
/// bandwidth either end has. Forcing HTTP/1.1 on the same link measured about
/// four times that, with restic using one socket per concurrent upload.
///
/// It has to be fixed here, on the server. restic configures HTTP/2 through
/// `golang.org/x/net/http2`, which puts `h2` in the TLS ALPN offer
/// unconditionally and never consults `GODEBUG=http2client=0`; there is no
/// restic option either. But ALPN is the server's choice. rest-server uses the
/// standard library's built-in HTTP/2, which does honour `http2server=0`, and a
/// server that does not offer `h2` leaves the client on HTTP/1.1.
///
/// This only governs a rest-server terminating TLS itself. A reverse proxy in
/// front of it negotiates ALPN on its own, and the README covers that case.
const SERVE_HTTP1_ONLY: &str = "GODEBUG=http2server=0";

/// Create a login, restart, and verify it before handing it to anyone.
///
/// rest-server loads .htpasswd ONCE at startup and never reloads it. Its own
/// log says so: "Loaded htpasswd file /data/.htpasswd". A credential created
/// after the server is running returns 401 until the container restarts, which
/// looks exactly like a wrong password and sends both sides off debugging TLS.
///
/// So this exists purely to make the correct sequence unavoidable. Returns the
/// generated password when it generated one.
pub fn adduser(
    ctx: &Ctx,
    peer: &PeerName,
    password: Option<&str>,
    o: &ServerOpts,
) -> Result<Option<String>, String> {
    let (pw, generated) = match password {
        Some(p) => (check_password(p)?.to_owned(), false),
        None => {
            let p = random_token(24).map_err(|e| format!("could not generate a password: {e}"))?;
            (p, true)
        }
    };

    // `htpasswd` overwrites an existing entry without a word. `quickstart` calls
    // this, so running `host quickstart alice` a second time -- to check on it,
    // or after a failure elsewhere -- silently replaced alice's password with a
    // new one. Her backups then fail with 401 until someone works out that the
    // credential she was given is no longer the credential the server holds.
    if !ctx.force && has_login(&o.container, peer) {
        return Err(format!(
            "'{peer}' already has a login on '{}'.\n       \
             Creating one again replaces their password, and everything they run \
             with the\n       old one starts failing with 401.\n\n       \
             If that is what you want: peerbackup host adduser {peer} --force\n       \
             If you only wanted to check the server is up: peerbackup host list",
            o.container
        ));
    }

    if generated {
        ctx.say(&format!("generated password for '{peer}': {pw}"));
        ctx.say("send it over a channel you trust, apart from the URL. It is not stored");
        ctx.say("anywhere in plaintext, so this is the only time it is shown.");
    }

    if ctx.would(&format!(
        "docker exec -i {} htpasswd -B -i $PASSWORD_FILE {peer}",
        o.container
    )) {
        ctx.would(&format!("docker restart {}", o.container));
        return Ok(generated.then_some(pw));
    }

    let out = Command::new("docker")
        // `htpasswd -i` reads the password from stdin, which is what the image's
        // own `create_user <name> <password>` would have put in argv, readable
        // by any process on the host through /proc/<pid>/cmdline for the life of
        // the call. This is the password that decrypts a friend's whole
        // repository; the restic side already avoids argv and env for the same
        // reason.
        //
        // This does what create_user does, minus the argv. -B is bcrypt, as the
        // image's script uses, and $PASSWORD_FILE is set in the image so the
        // file location stays the image's business rather than ours.
        .args([
            "exec",
            "-i",
            &o.container,
            "sh",
            "-c",
            r#"htpasswd -B -i "$PASSWORD_FILE" "$1""#,
            "sh",
            peer.as_str(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run docker: {e}"))
        .and_then(|mut child| {
            use std::io::Write as _;
            if let Some(mut sink) = child.stdin.take() {
                let _ = sink.write_all(pw.as_bytes());
                let _ = sink.write_all(b"\n");
            }
            child
                .wait_with_output()
                .map_err(|e| format!("could not read docker's output: {e}"))
        })?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        warn(msg.trim());
        return Err(format!(
            "could not create the login for '{peer}' on container '{}'.\n       \
             If the message above mentions /data, the server's storage directory is\n       \
             missing or not writable. Check what it is mounted on:\n         \
             docker inspect -f '{{{{range .Mounts}}}}{{{{.Source}}}} -> {{{{.Destination}}}}{{{{end}}}}' {}",
            o.container, o.container
        ));
    }

    ctx.say("restarting the server so it picks up the new credential (it only reads");
    ctx.say("the htpasswd file at startup)");
    // One server serves every peer, so this interrupts anyone mid-upload. restic
    // resumes -- the repository is append-only and a partial upload leaves no
    // snapshot -- but a friend seventeen hours into a 300GB seed would rather
    // know than wonder.
    ctx.say("this drops any transfer in progress; peers retry, but a large first");
    ctx.say("backup will lose its place");
    ctx.run("docker", &["restart", &o.container])?;

    if ctx.dry_run {
        return Ok(generated.then_some(pw));
    }

    // 404 means authenticated but no repository yet, which is exactly right for
    // a fresh grant. 200 means they already have one.
    let mut code = String::new();
    for _ in 0..40 {
        code = http_status(
            o.port,
            &format!("/{peer}/config"),
            Some((peer.as_str(), &pw)),
        );
        if code == "404" || code == "200" {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    match code.as_str() {
        "404" | "200" => {
            ctx.say(&format!(
                "verified: credential for '{peer}' authenticates (HTTP {code})"
            ));
            Ok(generated.then_some(pw))
        }
        "401" => Err(format!(
            "the login for '{peer}' still fails after a restart. The server is\n       \
             running but does not accept it; check its htpasswd file."
        )),
        _ => Err(format!(
            "could not confirm the login for '{peer}' works (HTTP {}).\n       \
             Nothing is answering on port {}, so the server is not serving there.\n       \
             Check it:   docker ps --filter name={}\n       \
             Remove it:  docker rm -f {}",
            if code.is_empty() {
                "no response"
            } else {
                &code
            },
            o.port,
            o.container,
            o.container
        )),
    }
}

/// Which compose file `host up`/`host down` should drive.
///
/// As a binary there is no script directory to hang this off, so the file is
/// looked up where a person would keep it: named outright, or beside them.
/// Takes the override rather than reading it, so the resolution order is
/// testable without setting an environment variable from a test -- which races
/// every other test in the binary.
fn compose_file(override_path: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p);
    }
    let here = PathBuf::from("compose.yml");
    if here.exists() {
        return Ok(here);
    }
    Err(
        "no compose.yml here. Run this from the directory holding it, or set \
         PB_COMPOSE_FILE=/path/to/compose.yml"
            .into(),
    )
}

/// Refuse a supplied password that the two things downstream cannot carry.
///
/// The password reaches `htpasswd -i` as one line on stdin, and reaches curl as
/// one `user = "..."` line in a config file. Both are line-oriented, so an
/// embedded newline silently truncates: `htpasswd` stores everything before it,
/// while the operator believes the whole string is the credential and sends that
/// to their friend. The verification step then reports a puzzling 401 for a
/// login that was, from its own point of view, created successfully.
///
/// Generated passwords are alphanumeric and cannot trip this. It exists for the
/// `host adduser <peer> <password>` path, where the value comes from a human.
fn check_password(pw: &str) -> Result<&str, String> {
    if pw.is_empty() {
        return Err("the password is empty; omit the argument to generate one".into());
    }
    if let Some(c) = pw.chars().find(|c| c.is_control()) {
        return Err(format!(
            "the password contains a control character ({}), which cannot be stored.\n       \
             htpasswd reads one line from stdin, so everything from there on would be\n       \
             dropped and the stored login would not match what you send your friend.\n       \
             Omit the argument to generate one instead.",
            c.escape_default()
        ));
    }
    Ok(pw)
}

pub fn compose(ctx: &Ctx, up: bool) -> Res {
    let file = compose_file(std::env::var_os("PB_COMPOSE_FILE").map(PathBuf::from))?;
    let f = file.to_string_lossy().into_owned();
    if up {
        ctx.run("docker", &["compose", "-f", &f, "up", "-d"])
    } else {
        ctx.run("docker", &["compose", "-f", &f, "down"])
    }
}

// ------------------------------------------------------------------- plumbing

fn writable(path: &std::path::Path) -> bool {
    let probe = path.join(format!(".peerbackup-write-test-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn docker_ok() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn container_running(name: &str) -> bool {
    Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == name)
        })
        .unwrap_or(false)
}

/// True when nothing is listening on the port.
fn port_free(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(300),
    )
    .is_err()
}

fn http_reachable(port: u16) -> bool {
    !http_status(port, "/", None).is_empty()
}

/// The status code as a string, or empty when nothing answered.
///
/// curl rather than an HTTP crate: this is two calls on a setup path, and the
/// alternative is a dependency tree larger than the rest of the program.
fn http_status(port: u16, path: &str, auth: Option<(&str, &str)>) -> String {
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "5",
    ]);
    // Credentials go in on stdin via `--config -`, never in argv. Anything in
    // argv is readable by any process on the machine through /proc/<pid>/cmdline
    // for as long as the call runs, and this is the password that decrypts a
    // friend's whole repository. The restic side already got this right by
    // passing the password as a file rather than an env var.
    if auth.is_some() {
        cmd.args(["--config", "-"]);
    }
    cmd.arg(&url);

    let Some((user, pw)) = auth else {
        return run_for_status(cmd, None);
    };
    // curl's config format. The value is quoted, so a backslash or a quote in
    // the password has to be escaped or it would end the string early.
    let escaped = pw.replace('\\', "\\\\").replace('"', "\\\"");
    run_for_status(cmd, Some(format!("user = \"{user}:{escaped}\"\n")))
}

fn run_for_status(mut cmd: Command, stdin_text: Option<String>) -> String {
    use std::io::Write as _;
    use std::process::Stdio;

    cmd.stdin(if stdin_text.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());

    let Ok(mut child) = cmd.spawn() else {
        return String::new();
    };
    if let (Some(text), Some(mut sink)) = (stdin_text, child.stdin.take()) {
        let _ = sink.write_all(text.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => {
            let code = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if code == "000" { String::new() } else { code }
        }
        Err(_) => String::new(),
    }
}

fn hostname() -> String {
    for args in [vec!["-f"], vec![]] {
        if let Ok(o) = Command::new("hostname").args(&args).output() {
            let h = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if o.status.success() && !h.is_empty() {
                return h;
            }
        }
    }
    "localhost".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_nobody_listens_on_reads_as_free() {
        // Port 1 requires root to bind and is not in use on a test machine.
        assert!(port_free(1));
    }

    #[test]
    fn a_bound_port_does_not_read_as_free() {
        let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(!port_free(port), "a listening socket must not read as free");
    }

    #[test]
    fn a_writable_directory_reads_as_writable_and_leaves_nothing_behind() {
        let d = std::env::temp_dir().join(format!("pb-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(writable(&d));
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            leftovers.is_empty(),
            "write probe must clean up after itself"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_compose_file_override_wins_and_a_missing_one_says_what_to_do() {
        let named = PathBuf::from("/somewhere/else/compose.yml");
        assert_eq!(compose_file(Some(named.clone())).unwrap(), named);
        // The override is taken as given: naming a file that is not there must
        // fail in `docker compose`, which says which file, rather than silently
        // falling back to whatever happens to be in the current directory.
        assert_eq!(
            compose_file(Some(PathBuf::from("/no/such/compose.yml"))).unwrap(),
            PathBuf::from("/no/such/compose.yml")
        );
        // With no override and no file beside us, the error has to name the way
        // out. Tests run from the repository root, where compose.yml exists, so
        // only assert the message when it genuinely is not there.
        if !PathBuf::from("compose.yml").exists() {
            let e = compose_file(None).unwrap_err();
            assert!(e.contains("PB_COMPOSE_FILE"), "got: {e}");
        } else {
            assert_eq!(compose_file(None).unwrap(), PathBuf::from("compose.yml"));
        }
    }

    #[test]
    fn a_password_that_cannot_survive_htpasswd_is_refused_up_front() {
        // htpasswd -i reads one line. A password with a newline in it was stored
        // truncated while the operator sent their friend the whole string, and
        // the only symptom was a 401 from a login that had just been "verified".
        for bad in [
            "with\nnewline",
            "with\rreturn",
            "tab\there",
            "nul\0byte",
            "",
        ] {
            assert!(check_password(bad).is_err(), "{bad:?} must be refused");
        }
        // Everything a human might reasonably pick still works, including the
        // shell-hostile characters, because nothing here goes through a shell.
        for good in [
            "hunter2",
            "p@ssw0rd",
            "a b c",
            "\"quoted\"",
            "back\\slash",
            "£10",
        ] {
            assert_eq!(check_password(good).unwrap(), good);
        }
    }

    #[test]
    fn a_missing_directory_is_not_writable() {
        assert!(!writable(std::path::Path::new("/nope/not/here")));
    }

    #[test]
    fn the_server_is_started_without_http2() {
        // HTTP/2 puts every upload on one TCP connection, which caps a distant
        // peer at a fraction of the link. restic's client cannot be told to
        // avoid it, so the server must not offer it.
        let o = ServerOpts {
            port: 51515,
            data: PathBuf::from("/srv/data"),
            container: "peerbackup-rest".into(),
            max_size: 1,
        };
        let args = server_run_args(&o, "1000:1000");
        let pos = args
            .iter()
            .position(|a| a == "GODEBUG=http2server=0")
            .expect("the server must be told not to offer HTTP/2");
        assert_eq!(
            args[pos - 1],
            "-e",
            "it must arrive as an environment variable"
        );
        assert_eq!(args.last().unwrap(), REST_SERVER_IMAGE, "image stays last");
    }

    #[test]
    fn quickstart_cannot_be_handed_a_name_that_would_break_a_path() {
        // Was `quickstart(&ctx, "a b", &o)`, asserting an error. That no longer
        // compiles -- the parameter is a validated `PeerName` -- so what is left
        // to check is that the container name and invite URL are built from a
        // name the type already vouched for.
        let ctx = Ctx {
            root: PathBuf::from("/tmp/pb-unused"),
            units: PathBuf::from("/tmp/pb-unused"),
            reserve_pct: 15,
            host_margin_gb: 20,
            dry_run: true,
            quiet: true,
            force: false,
        };
        let o = ServerOpts {
            port: 51515,
            data: PathBuf::from("/tmp/pb-unused-data"),
            container: "x".into(),
            max_size: 1,
        };
        assert!(PeerName::new("a b").is_err());
        assert!(PeerName::new("").is_err());
        // A valid name gets past validation. On a machine with docker the dry
        // run then succeeds; on one without, the complaint is about docker and
        // never about the name.
        if let Err(e) = quickstart(&ctx, &PeerName::new("alice").unwrap(), &o) {
            assert!(
                e.contains("docker"),
                "a legitimate name must not be refused as a name: {e}"
            );
        }
    }

    #[test]
    fn a_dry_run_of_quickstart_does_not_touch_anything() {
        // `quickstart` and `adduser` built their docker calls with a raw
        // `Command::new` instead of `ctx.run`, so `Ctx::dry_run` was never
        // consulted anywhere in the server path -- and the contract on it says
        // "print what would happen and touch nothing".
        //
        // A rehearsal with a stopped container really ran `docker rm -f`,
        // taking its configuration with it, then really started a new one and
        // really wrote an htpasswd entry. This test itself was the proof: it
        // ran during `cargo test` on any machine with docker, deleted whatever
        // was named `x`, and left a rest-server listening on 51515 with
        // `--restart unless-stopped`.
        let dir = std::env::temp_dir().join(format!("pb-dryrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ctx = Ctx {
            root: PathBuf::from("/tmp/pb-unused"),
            units: PathBuf::from("/tmp/pb-unused"),
            reserve_pct: 15,
            host_margin_gb: 20,
            dry_run: true,
            quiet: true,
            force: false,
        };
        let o = ServerOpts {
            port: 51515,
            data: dir.clone(),
            container: "peerbackup-test-must-not-exist".into(),
            max_size: 1,
        };
        let _ = quickstart(&ctx, &PeerName::new("alice").unwrap(), &o);
        assert!(
            !dir.exists(),
            "a dry run created the storage directory at {}",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
