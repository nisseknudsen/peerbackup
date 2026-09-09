//! Storage grants: create one, inspect it, refuse to serve a broken one, give
//! it back.
//!
//! ```text
//!   provision                          release
//!   ---------                          -------
//!   admission control                  confirm (name must be typed)
//!   fallocate            <- reserves   systemctl disable --now
//!   mkfs.ext4 -E nodiscard             umount        <- order matters:
//!   verify allocation    <- AFTER      losetup -d       rm on a mounted image
//!   write mount unit                   rm image         leaves the space
//!   systemctl enable --now             rmdir            allocated to the open
//!   chown parent, then grant -R        rm unit          loop device
//! ```

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::size::{human, parse_size};
use super::{Ctx, Res, have, is_mountpoint, statfs, unit_name, warn};
use crate::config::PeerName;

/// Tools provisioning cannot do without, checked before anything is created.
///
/// systemd-escape used to be missing silently: the unit name came back empty,
/// the unit was written to a garbage path, and provision reported success on a
/// grant that would never mount.
const REQUIRED: [&str; 4] = ["fallocate", "mkfs.ext4", "systemd-escape", "losetup"];

pub fn provision(ctx: &Ctx, peer: &PeerName, size: &str) -> Res {
    ctx.need_root()?;

    let img = ctx.image_of(peer.as_str());
    let dir = ctx.dir_of(peer.as_str());
    if img.exists() {
        return Err(format!(
            "{} already exists; use `peerbackup host release {peer}` first if you mean to replace it",
            img.display()
        ));
    }

    let bytes = parse_size(size)?;

    // Admission control. Preallocation means the host must actually have the
    // room right now, not eventually. Refuse rather than overcommit.
    //
    // This runs under DRY_RUN too, deliberately. A dry run that skips the one
    // safety check you would want before committing 500GB is not a dry run, it
    // is a rehearsal of the happy path.
    let images = ctx.images();
    let mut probe = images.as_path();
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p,
            None => break,
        }
    }
    let (_, avail) = statfs(probe)?;
    let margin = ctx.host_margin_gb * 1024 * 1024 * 1024;
    ctx.info(&format!("requested:      {}", human(bytes)));
    ctx.info(&format!(
        "host available: {}  (on {})",
        human(avail),
        probe.display()
    ));
    ctx.info(&format!("safety margin:  {}", human(margin)));
    if bytes.saturating_add(margin) > avail {
        return Err(format!(
            "refusing to overcommit: {} + {} margin exceeds {} available on {}",
            human(bytes),
            human(margin),
            human(avail),
            probe.display()
        ));
    }

    let missing: Vec<&str> = REQUIRED.iter().copied().filter(|t| !have(t)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required tool(s): {} (need util-linux, e2fsprogs and systemd)",
            missing.join(" ")
        ));
    }

    if !ctx.would(&format!(
        "mkdir -p {} {}",
        images.display(),
        ctx.mnt().display()
    )) {
        for d in [&images, &ctx.mnt()] {
            std::fs::create_dir_all(d)
                .map_err(|e| format!("could not create {}: {e}", d.display()))?;
        }
    }

    ctx.info(&format!(
        "allocating {} (preallocated, this is not instant)",
        img.display()
    ));
    ctx.run(
        "fallocate",
        &[
            OsStr::new("-l"),
            OsStr::new(&bytes.to_string()),
            img.as_os_str(),
        ],
    )
    .map_err(|e| {
        format!(
            "{e}\n       fallocate failed. The filesystem may not support preallocation; \
                 dd is NOT equivalent, investigate before working around it."
        )
    })?;

    // -m 0 drops ext4's default 5% root reservation. The peer paid for that
    // space.
    //
    // -E nodiscard is NOT optional and is the subtlest line in this file.
    // mkfs.ext4 issues discards by default, which on a file-backed image
    // punches holes and silently undoes the fallocate above:
    //
    //     after fallocate:           allocated = 64M
    //     after mkfs.ext4 (default): allocated = 4.5M   <- preallocation gone
    //     after mkfs -E nodiscard:   allocated = 64M
    //
    // Without it every grant is sparse, and the host can be overcommitted by
    // exactly the mechanism this design exists to prevent.
    // `?` here would return with the image already allocated. `mkfs` fails for
    // ordinary reasons -- a size ext4 cannot format, an interrupted run -- and
    // the only way out of a stranded image was `host release`, the
    // type-the-name-to-confirm destructor whose own warning is that it
    // permanently destroys the peer's backups. Up to the whole grant, e.g.
    // 500GB, left behind by a failure that touched nothing else.
    if let Err(e) = ctx.run(
        "mkfs.ext4",
        &[
            OsStr::new("-q"),
            OsStr::new("-m"),
            OsStr::new("0"),
            OsStr::new("-E"),
            OsStr::new("nodiscard"),
            OsStr::new("-F"),
            img.as_os_str(),
        ],
    ) {
        remove_quietly(ctx, &img);
        return Err(format!(
            "{e}\n       The image was removed, so this can be run again once the \
             cause is fixed."
        ));
    }

    verify_not_sparse(ctx, &img)?;

    // Everything from here can fail with the image already on disk. Without a
    // rollback the next `provision` refuses ("already exists; use release"),
    // and `release` is the type-the-name-to-confirm destructor whose own
    // warning is that it permanently destroys the peer's backups. Recovering
    // from a half-provisioned grant should not require running that.
    //
    // The boundary starts here, not after the mount unit is written. It used to
    // begin three steps later, so creating the mount directory, deriving the
    // unit name and writing the unit all returned early with a fully
    // preallocated image -- up to the whole grant, e.g. 500GB -- left behind
    // and no way forward except the destructor. `verify_not_sparse` removes the
    // image on its own failure path, which is what makes this the right seam.
    let finish = |unit: &mut String| -> Res {
        check_mount_target(&dir)?;
        if !ctx.would(&format!("mkdir -p {}", dir.display())) {
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
            check_mount_target(&dir)?;
        }

        *unit = unit_name(&dir)?;
        let unit_path = ctx.units.join(&*unit);
        ctx.info(&format!("writing {}", unit_path.display()));
        if ctx.dry_run {
            println!(
                "  would write mount unit for {} -> {}",
                img.display(),
                dir.display()
            );
        } else {
            let text = format!(
                "[Unit]\n\
                 Description=peerbackup storage grant for {peer}\n\
                 Documentation=https://github.com/nisseknudsen/peerbackup\n\
                 \n\
                 [Mount]\n\
                 What={}\n\
                 Where={}\n\
                 Type=ext4\n\
                 Options=loop,rw,noatime\n\
                 \n\
                 [Install]\n\
                 WantedBy=multi-user.target\n",
                img.display(),
                dir.display()
            );
            std::fs::create_dir_all(&ctx.units)
                .and_then(|()| std::fs::write(&unit_path, text))
                .map_err(|e| format!("could not write {}: {e}", unit_path.display()))?;
        }

        ctx.run("systemctl", &["daemon-reload"])?;
        ctx.run("systemctl", &["enable", "--now", unit])?;
        take_ownership(ctx, &dir, unit)
    };

    // Carried out of the closure so the rollback can undo a unit that was named
    // and possibly written before the step that failed.
    let mut unit = String::new();
    if let Err(e) = finish(&mut unit) {
        warn("provisioning failed part-way; undoing what was created");
        // Same order as `release`, and for the reason written down there:
        // unlinking a mounted image is not a teardown. The space stays allocated
        // to the open loop device and the mount goes on serving, so the next
        // `provision` allocates a second image beside a first one that nothing
        // can see. This skipped straight to `rm`.
        if !unit.is_empty() {
            ctx.run_best_effort("systemctl", &["disable", "--now", &unit]);
            remove_quietly(ctx, &ctx.units.join(&unit));
        }
        let mut stranded = Vec::new();
        if is_mountpoint(&dir) && ctx.run("umount", &[dir.as_os_str()]).is_err() {
            stranded.push(format!("{} is still mounted", dir.display()));
        }
        for loopdev in loop_devices_for(&img) {
            if ctx.run("losetup", &["-d", &loopdev]).is_err() {
                stranded.push(format!("{loopdev} is still attached to the image"));
            }
        }
        if stranded.is_empty() {
            remove_quietly(ctx, &img);
            if !ctx.would(&format!("rmdir {}", dir.display())) {
                let _ = std::fs::remove_dir(&dir);
            }
        }
        ctx.run_best_effort("systemctl", &["daemon-reload"]);
        // Claimed unconditionally before, including on the paths where it was
        // not true.
        return Err(if stranded.is_empty() {
            format!(
                "{e}\n       Nothing was left behind, so this can be run again once \
                 the cause is fixed."
            )
        } else {
            format!(
                "{e}\n       Could not fully undo it: {}.\n       \
                 The image is still at {} and is still using its space. Clear that \
                 first;\n       `peerbackup host release {peer}` will do it.",
                stranded.join(", "),
                img.display()
            )
        });
    }

    ctx.info("");
    ctx.info(&format!(
        "grant created for '{peer}'. Numbers that matter, all three of them:"
    ));
    list(ctx, Some(peer.as_str()))?;
    ctx.info("");
    ctx.info("next: create their credential");
    ctx.info(&format!("  peerbackup host adduser {peer}"));
    ctx.info("(use that, not create_user directly: rest-server only reads the htpasswd");
    ctx.info(" file at startup, so a credential added without a restart returns 401)");
    Ok(())
}

/// Refuse to mount over anything that is not a real, empty, unmounted directory.
///
/// `take_ownership` hands the *parent* of every grant directory to an
/// unprivileged uid, so `/srv/peerbackup/mnt` is writable by an account that is
/// not root. `create_dir_all` then treats an existing symlink-to-a-directory as
/// success, `systemd-escape` escapes the literal path, and `mount` canonicalises
/// symlinks -- so someone with a foothold in that account could plant
/// `ln -s /etc /srv/peerbackup/mnt/carol`, wait for the admin to run
/// `sudo peerbackup host provision carol 500G`, and get a blank ext4 mounted
/// over `/etc`.
///
/// `symlink_metadata` does not follow the link, which is the whole point.
/// Checking again after `create_dir_all` closes the window between the two.
///
/// Emptiness matters for a different reason: mounting over a directory that
/// already holds files hides them for as long as the mount lasts, and nothing
/// in `host list` would say so.
fn check_mount_target(dir: &Path) -> Res {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        // Absent is fine; it is about to be created.
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symbolic link, and mounting follows it.\n       \
             Remove it and run this again. Nothing here should be a link: this \
             directory's\n       parent is writable by a non-root account, which \
             is how a link pointing\n       somewhere important gets planted.",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "{} exists and is not a directory, so it cannot be a mount point",
            dir.display()
        ));
    }
    if is_mountpoint(dir) {
        return Err(format!(
            "{} is already a mount point. Run `peerbackup host list` to see what \
             this\n       host thinks is there.",
            dir.display()
        ));
    }
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(format!(
                    "{} is not empty, and mounting over it would hide what is in \
                     it.\n       Move that aside first.",
                    dir.display()
                ));
            }
            Ok(())
        }
        Err(e) => Err(format!("could not read {}: {e}", dir.display())),
    }
}

/// Check what was actually shipped, not an intermediate state.
///
/// The first version of this check ran immediately after `fallocate`, passed,
/// and was then invalidated by the very next command. Preallocation is only
/// real if it survives mkfs.
fn verify_not_sparse(ctx: &Ctx, img: &Path) -> Res {
    if ctx.dry_run {
        return Ok(());
    }
    let m = std::fs::metadata(img)
        .map_err(|e| format!("could not stat {} after mkfs: {e}", img.display()))?;
    let apparent = m.size();
    // st_blocks is in 512-byte units by POSIX, whatever the filesystem's own
    // block size happens to be.
    let allocated = m.blocks() * 512;
    // 95%, not 50%. A grant is a hard reservation or it is nothing, and a
    // filesystem that half-honours `fallocate` -- or an image whose allocation
    // was partly discarded -- is exactly the overcommit this check exists to
    // catch. At 50% a 500GB grant backed by 255GB passed and printed
    // "preallocation verified". With `-E nodiscard` on a working filesystem the
    // ratio is ~1.0, so the margin only has to cover filesystem metadata.
    if allocated < apparent / 100 * 95 {
        let _ = std::fs::remove_file(img);
        return Err(format!(
            "refusing to create a sparse grant: only {} of {} is actually allocated.\n       \
             A sparse image is not a quota. The host can still be filled by other grants.\n       \
             Either {} is on a filesystem that does not really preallocate\n       \
             (overlayfs, tmpfs, some network mounts), or mkfs discarded the allocation.\n       \
             Put it on ext4, xfs or btrfs on real block storage.",
            human(allocated),
            human(apparent),
            ctx.images().display()
        ));
    }
    ctx.info(&format!(
        "preallocation verified after mkfs: {} reserved on the host",
        human(allocated)
    ));
    Ok(())
}

/// Hand the grant to the user the server runs as.
///
/// A freshly formatted filesystem is owned by root, and mkfs.ext4 always
/// creates lost+found as root:0700 whatever the parent looks like. The server
/// runs unprivileged, so without this it cannot write to the grant at all, and
/// both `adduser` and the startup mount check fail with permission denied.
///
/// Both paths matter. The parent holds rest-server's .htpasswd: left as root,
/// every `adduser` fails, which is the first thing anyone hits. And `-R` on the
/// grant is required rather than tidy, because lost+found alone breaks it.
fn take_ownership(ctx: &Ctx, dir: &Path, unit: &str) -> Res {
    // PB_UID wins so it can match the service file. Otherwise the person who
    // ran sudo, since they are the one who will want to inspect the data later.
    // SAFETY: getuid/getgid read process properties and cannot fail.
    // Parsed rather than interpolated: a non-numeric PB_UID is not exploitable
    // (nothing goes through a shell) but it produces an opaque chown error
    // several steps later instead of naming the variable that is wrong.
    let uid = numeric_env(&["PB_UID", "SUDO_UID"])?.unwrap_or_else(|| unsafe { libc::getuid() });
    let gid = numeric_env(&["PB_GID", "SUDO_GID"])?.unwrap_or_else(|| unsafe { libc::getgid() });
    let own = format!("{uid}:{gid}");
    let mnt = ctx.mnt();

    if ctx.dry_run {
        println!("  would run: chown {own} {}", mnt.display());
        println!("  would run: chown -R {own} {}", dir.display());
        return Ok(());
    }

    ctx.run("chown", &[OsStr::new(&own), mnt.as_os_str()])
        .map_err(|e| {
            format!(
                "{e}\n       could not give {} to {own}; creating logins will fail",
                mnt.display()
            )
        })?;

    if !is_mountpoint(dir) {
        return Err(format!(
            "{} is not mounted after enabling {unit}, so it cannot be prepared.\n       \
             Check: systemctl status {unit}",
            dir.display()
        ));
    }

    ctx.run(
        "chown",
        &[OsStr::new("-R"), OsStr::new(&own), dir.as_os_str()],
    )
    .map_err(|e| {
        format!(
            "{e}\n       could not give {} to {own}; the server cannot write to it",
            dir.display()
        )
    })?;
    ctx.info(&format!("grant and {} owned by {own}", mnt.display()));
    Ok(())
}

pub fn release(ctx: &Ctx, peer: &PeerName) -> Res {
    ctx.need_root()?;

    let img = ctx.image_of(peer.as_str());
    let dir = ctx.dir_of(peer.as_str());
    // Before deriving the unit name, which shells out: on a machine without
    // systemd-escape, `release nobody` should say there is no such grant rather
    // than report a systemd-escape failure.
    if !img.exists() && !dir.is_dir() {
        return Err(format!("no grant found for '{peer}'"));
    }
    let unit = unit_name(&dir)?;

    warn(&format!(
        "this destroys {peer}'s backups permanently. They cannot be recovered from here."
    ));
    if !ctx.force && !ctx.dry_run {
        use std::io::Write;
        print!("type the peer name to confirm: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("could not read confirmation: {e}"))?;
        if line.trim() != peer.as_str() {
            return Err("aborted".into());
        }
    }

    // Order matters. `rm` on a mounted image is not a teardown: the space stays
    // allocated to the open loop device and the mount keeps serving stale data.
    ctx.run_best_effort("systemctl", &["disable", "--now", &unit]);
    if is_mountpoint(&dir) {
        ctx.run("umount", &[dir.as_os_str()]).map_err(|e| {
            format!(
                "{e}\n       something still has {} open (lsof +f -- {})",
                dir.display(),
                dir.display()
            )
        })?;
    }
    // Not best-effort. With the container running, its bind mount holds the loop
    // device open and `losetup -d` fails with EBUSY -- and the image was then
    // unlinked anyway, so the space stayed allocated to a device with no name.
    // "capacity returned to the host" was printed regardless.
    let mut still_attached = Vec::new();
    for loopdev in loop_devices_for(&img) {
        if let Err(e) = ctx.run("losetup", &["-d", &loopdev]) {
            warn(&e);
            still_attached.push(loopdev);
        }
    }
    if !still_attached.is_empty() {
        return Err(format!(
            "the grant for '{peer}' was not released: {} still attached to {}.\n       \
             Removing the image now would leak the space rather than return it.\n       \
             Something has it open -- the server container is the usual answer.\n       \
             Stop it and run this again.",
            still_attached.join(", "),
            img.display()
        ));
    }
    // std::fs rather than shelling out: these are three syscalls, and going
    // through a process each time only adds a PATH lookup and an error string
    // to re-parse. `unit_name` shells out for a real reason; these did not.
    remove_quietly(ctx, &img);
    if !ctx.would(&format!("rmdir {}", dir.display())) {
        let _ = std::fs::remove_dir(&dir);
    }
    remove_quietly(ctx, &ctx.units.join(&unit));
    ctx.run("systemctl", &["daemon-reload"])?;
    ctx.info(&format!(
        "grant for '{peer}' released, capacity returned to the host"
    ));
    Ok(())
}

/// Remove a file, saying so under a dry run and shrugging if it is not there.
fn remove_quietly(ctx: &Ctx, path: &Path) {
    if ctx.would(&format!("rm -f {}", path.display())) {
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn(&format!("could not remove {}: {e}", path.display())),
    }
}

/// Every loop device backed by this image, not just the first.
///
/// `losetup -j` prints one line per attachment and nothing stops there being
/// more than one -- a stale attachment from an earlier run, or a second
/// `losetup -f` by hand. Taking only the first line meant `release` detached one
/// device, unlinked the image, and left the rest holding its space with no name
/// left to find them by.
///
/// Also ignores `losetup`'s exit status on purpose: a non-zero exit with no
/// output is "no devices", which is the normal case for a grant that was never
/// mounted.
fn loop_devices_for(img: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("losetup")
        .arg("-j")
        .arg(img)
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let dev = l.split(':').next()?.trim();
            (!dev.is_empty()).then(|| dev.to_owned())
        })
        .collect()
}

pub fn list(ctx: &Ctx, only: Option<&str>) -> Res {
    let mnt = ctx.mnt();
    if !mnt.is_dir() {
        ctx.info("no grants");
        return Ok(());
    }
    let mut dirs: Vec<_> = std::fs::read_dir(&mnt)
        .map_err(|e| format!("could not read {}: {e}", mnt.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    let mut found = false;
    for dir in dirs {
        let peer = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if only.is_some_and(|o| o != peer) {
            continue;
        }
        if !found {
            // Printed here rather than up front so `list` on an empty host says
            // "no grants" without a header above it, and so `provision` -- which
            // calls this for one peer -- does not print a table it will not fill.
            println!(
                "{:<14} {:>12} {:>12} {:>12} {:>12}  STATE",
                "PEER", "IMAGE", "USABLE", "RESERVE", "USED"
            );
        }
        found = true;
        let img = ctx.image_of(&peer);
        let imgsz = std::fs::metadata(&img).map(|m| m.size()).unwrap_or(0);
        let (mut usable, mut used, mut reserve) = (0u64, 0u64, 0u64);
        let state = if is_mountpoint(&dir) {
            if let Ok((total, avail)) = statfs(&dir) {
                usable = total;
                used = total.saturating_sub(avail);
                reserve = total * ctx.reserve_pct / 100;
            }
            "mounted"
        } else {
            "NOT MOUNTED"
        };
        println!(
            "{:<14} {:>12} {:>12} {:>12} {:>12}  {}",
            peer,
            human(imgsz),
            human(usable),
            human(reserve),
            human(used),
            state
        );
    }
    if !found {
        ctx.info("no grants");
        return Ok(());
    }
    ctx.info("");
    ctx.info("USABLE is capacity after filesystem overhead, which is less than IMAGE.");
    ctx.info(&format!(
        "RESERVE ({}%) is how much headroom prune needs to repack.",
        ctx.reserve_pct
    ));
    ctx.info("Nothing enforces it today. It is shown so you can leave room by hand:");
    ctx.info("if USED climbs past USABLE minus RESERVE, prune may be unable to run.");
    ctx.info("rest-server's --max-size is per instance, not per peer, so it cannot");
    ctx.info("enforce it either.");
    Ok(())
}

/// Fail-closed. Runs as ExecStartPre on the container unit.
///
/// If Docker starts before the mounts settle, a bind mount resolves to an
/// ordinary directory on the root filesystem with no size limit, and the first
/// symptom is a full host disk. Refusing to start is the cheaper failure.
pub fn guard(ctx: &Ctx) -> Res {
    let mnt = ctx.mnt();
    if !mnt.is_dir() {
        return Err(format!(
            "{} does not exist; nothing provisioned",
            mnt.display()
        ));
    }
    let mut dirs: Vec<_> = std::fs::read_dir(&mnt)
        .map_err(|e| format!("could not read {}: {e}", mnt.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    let mut bad = 0usize;
    for dir in &dirs {
        if is_mountpoint(dir) {
            println!("  ok       {}", dir.display());
        } else {
            eprintln!("  NOT MOUNTED  {}", dir.display());
            bad += 1;
        }
    }
    if dirs.is_empty() {
        return Err(format!(
            "no grant directories under {}; refusing to start a server with nothing to serve",
            mnt.display()
        ));
    }
    if bad > 0 {
        return Err(
            "refusing to start: a grant directory is not a mountpoint, so writes would land \
             on the host root filesystem with no quota"
                .into(),
        );
    }
    ctx.info(&format!("all {} grant(s) mounted", dirs.len()));
    Ok(())
}

pub fn doctor(ctx: &Ctx) -> Res {
    let mut problems = 0usize;
    for (tool, pkg) in [
        ("docker", ""),
        ("fallocate", " (util-linux)"),
        ("mkfs.ext4", " (e2fsprogs)"),
        ("systemd-escape", ""),
    ] {
        if !have(tool) {
            warn(&format!("{tool} not found{pkg}"));
            problems += 1;
        }
    }
    if !ctx.root.is_dir() {
        warn(&format!(
            "{} does not exist yet (provision will create it)",
            ctx.root.display()
        ));
    }

    // The gotcha that silently breaks quota monitoring: the rest-server image
    // runs as uid 0 and creates repositories 0700 root:root through the bind
    // mount, so the host owner cannot read their own data to measure it.
    if let Ok(entries) = std::fs::read_dir(ctx.mnt()) {
        for grant in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            if !grant.is_dir() {
                continue;
            }
            if let Ok(repos) = std::fs::read_dir(&grant) {
                for repo in repos.filter_map(|e| e.ok()).map(|e| e.path()) {
                    if repo.is_dir() && std::fs::read_dir(&repo).is_err() {
                        warn(&format!(
                            "{} is not readable by this user -- the container is probably \
                             running without --user, which breaks quota monitoring",
                            repo.display()
                        ));
                        problems += 1;
                        break;
                    }
                }
            }
        }
    }

    if problems == 0 {
        ctx.info("host looks healthy");
        return Ok(());
    }
    // Exits non-zero so it can gate something. A diagnostic that always
    // succeeds cannot be used in a script, in CI, or as an ExecStartPre, which
    // is the job its sibling `guard` already does.
    Err(format!("{problems} problem(s) found"))
}

/// The first of these variables that is set, as a uid or gid.
fn numeric_env(keys: &[&str]) -> Result<Option<u32>, String> {
    first_numeric(keys.iter().map(|k| (*k, std::env::var(k).ok())))
}

/// The same, over values supplied by the caller.
///
/// Split out so the refusal can be tested. It read `std::env` directly, so the
/// only case a test could reach was "nothing is set" -- which is why the test
/// named after refusing a non-numeric id never exercised one. Setting an
/// environment variable from a test is a data race against every other test in
/// the binary, and `unsafe` in edition 2024 for exactly that reason, so the
/// answer is to take the values rather than to reach for them.
fn first_numeric<'a>(
    vars: impl IntoIterator<Item = (&'a str, Option<String>)>,
) -> Result<Option<u32>, String> {
    for (key, value) in vars {
        match value {
            Some(v) if !v.is_empty() => {
                return v
                    .parse()
                    .map(Some)
                    .map_err(|_| format!("{key}='{v}' is not a numeric id"));
            }
            _ => {}
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_planted_symlink_cannot_become_a_mount_point() {
        // `take_ownership` chowns the parent of every grant directory to an
        // unprivileged uid, and `create_dir_all` treats a symlink to a
        // directory as success. `ln -s /etc /srv/peerbackup/mnt/carol` followed
        // by `sudo peerbackup host provision carol 500G` mounted a blank ext4
        // over /etc.
        let dir = std::env::temp_dir().join(format!("pb-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let link = dir.join("carol");
        std::os::unix::fs::symlink(dir.join("real"), &link).unwrap();

        let e = check_mount_target(&link).unwrap_err();
        assert!(e.contains("symbolic link"), "got: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_or_empty_directory_is_a_fine_mount_point() {
        let dir = std::env::temp_dir().join(format!("pb-mnttarget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        check_mount_target(&dir.join("not-there")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        check_mount_target(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mounting_over_something_that_already_has_files_in_it_is_refused() {
        // The mount hides them for as long as it lasts, and nothing in
        // `host list` would say so.
        let dir = std::env::temp_dir().join(format!("pb-mntfull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("someones-backup"), b"x").unwrap();
        let e = check_mount_target(&dir).unwrap_err();
        assert!(e.contains("not empty"), "got: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_where_a_mount_point_should_be_is_refused() {
        let dir = std::env::temp_dir().join(format!("pb-mntfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("carol");
        std::fs::write(&f, b"x").unwrap();
        let e = check_mount_target(&f).unwrap_err();
        assert!(e.contains("not a directory"), "got: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ctx_in(dir: &Path) -> Ctx {
        Ctx {
            root: dir.to_path_buf(),
            units: dir.join("units"),
            reserve_pct: 15,
            host_margin_gb: 20,
            dry_run: true,
            quiet: false,
            force: false,
        }
    }

    fn pn(s: &str) -> PeerName {
        PeerName::new(s).unwrap()
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_bad_peer_name_cannot_reach_provisioning_at_all() {
        // This used to call `provision(&ctx, "../etc", "1G")` and assert it
        // returned an error. It no longer compiles: the entry points take a
        // validated `PeerName`, so a traversal cannot be constructed to hand
        // them. The guarantee moved from a runtime check repeated at each entry
        // point into the type, and this is what is left to assert -- that the
        // names which would break a path or a systemd unit are refused at
        // construction, before any host code sees them.
        for bad in ["../etc", "a b", "a/b", "a.b", "a;rm -rf /", "", "péer"] {
            assert!(PeerName::new(bad).is_err(), "{bad:?} must be refused");
        }
        for good in ["alice", "bob-2", "a_b", "A1"] {
            assert!(PeerName::new(good).is_ok(), "{good} should be allowed");
        }
    }

    #[test]
    fn provision_refuses_a_bad_size_before_touching_anything() {
        let dir = tmp("g2");
        let ctx = ctx_in(&dir);
        let err = provision(&ctx, &pn("alice"), "banana").unwrap_err();
        assert!(err.contains("bad size"), "got: {err}");
        assert!(!dir.exists());
    }

    #[test]
    fn admission_control_runs_even_in_dry_run() {
        // A dry run that skips the one check you would want before committing
        // 500GB is a rehearsal of the happy path, not a dry run.
        let dir = tmp("g3");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_in(&dir);
        let err = provision(&ctx, &pn("alice"), "900000G").unwrap_err();
        assert!(
            err.contains("refusing to overcommit"),
            "dry run must still refuse an impossible grant, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guard_fails_closed_on_a_directory_that_is_not_mounted() {
        let dir = tmp("g4");
        std::fs::create_dir_all(dir.join("mnt").join("alice")).unwrap();
        let ctx = ctx_in(&dir);
        let err = guard(&ctx).unwrap_err();
        assert!(err.contains("refusing to start"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guard_refuses_when_there_is_nothing_to_serve() {
        let dir = tmp("g5");
        std::fs::create_dir_all(dir.join("mnt")).unwrap();
        let ctx = ctx_in(&dir);
        let err = guard(&ctx).unwrap_err();
        assert!(err.contains("nothing to serve"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guard_refuses_when_the_mount_root_is_missing_entirely() {
        let dir = tmp("g6");
        let ctx = ctx_in(&dir);
        assert!(guard(&ctx).is_err());
    }

    #[test]
    fn list_says_so_when_there_are_no_grants() {
        let dir = tmp("g7");
        let ctx = ctx_in(&dir);
        assert!(list(&ctx, None).is_ok());
    }

    #[test]
    fn release_refuses_a_peer_with_no_grant() {
        let dir = tmp("g8");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_in(&dir);
        let err = release(&ctx, &pn("nobody")).unwrap_err();
        assert!(err.contains("no grant found"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doctor_exits_non_zero_when_it_finds_a_problem() {
        // A diagnostic that always succeeds cannot gate anything, which is the
        // job its sibling `guard` already does as an ExecStartPre.
        let dir = tmp("g10");
        let ctx = Ctx {
            root: dir.join("definitely-absent"),
            ..ctx_in(&dir)
        };
        // No docker or mkfs on a bare test runner is enough to trip it; if the
        // machine has everything, the missing root directory only warns, so
        // accept either outcome rather than asserting on the environment.
        let result = doctor(&ctx);
        if let Err(e) = result {
            assert!(e.contains("problem"), "got: {e}");
        }
    }

    #[test]
    fn a_non_numeric_uid_is_refused_by_name() {
        // The point of parsing rather than interpolating: a bad PB_UID is not
        // exploitable, but it must name the variable that is wrong instead of
        // producing an opaque chown failure several steps later.
        let e = first_numeric([("PB_UID", Some("nisse".to_owned()))]).unwrap_err();
        assert!(e.contains("PB_UID"), "must name the variable: {e}");
        assert!(e.contains("nisse"), "must show the value: {e}");
    }

    #[test]
    fn the_first_variable_that_is_set_wins_and_empty_does_not_count() {
        // PB_UID wins over SUDO_UID so it can match the service file; an unset
        // or empty one falls through to the person who ran sudo.
        let pairs = |a: Option<&str>, b: Option<&str>| {
            [
                ("PB_UID", a.map(str::to_owned)),
                ("SUDO_UID", b.map(str::to_owned)),
            ]
        };
        assert_eq!(
            first_numeric(pairs(Some("1000"), Some("1001"))).unwrap(),
            Some(1000)
        );
        assert_eq!(
            first_numeric(pairs(None, Some("1001"))).unwrap(),
            Some(1001)
        );
        assert_eq!(
            first_numeric(pairs(Some(""), Some("1001"))).unwrap(),
            Some(1001)
        );
        assert_eq!(first_numeric(pairs(None, None)).unwrap(), None);
    }

    #[test]
    fn provision_refuses_to_replace_an_existing_image() {
        let dir = tmp("g9");
        std::fs::create_dir_all(dir.join("images")).unwrap();
        std::fs::write(dir.join("images").join("alice.img"), b"x").unwrap();
        let ctx = ctx_in(&dir);
        let err = provision(&ctx, &pn("alice"), "1G").unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
        assert!(
            err.contains("release alice"),
            "must name the way out: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
