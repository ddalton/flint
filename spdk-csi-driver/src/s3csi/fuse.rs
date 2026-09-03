//! The privileged part, kept small (design §3.4 steps 8-11, §3.6):
//! open `/dev/fuse`, perform the `mount(2)` as root, pass the fd to the
//! unprivileged worker over `SCM_RIGHTS`, bind the source into the
//! tenant's target path, and unmount on the way out.
//!
//! The FUSE mount is made on a plugin-owned SOURCE directory and BOUND
//! to the kubelet target, never mounted at the target directly: a dead
//! FUSE mount at a volume root wedges every later container creation
//! and pod deletion (`passthrough/inject.rs:57-85`, measured). The
//! source lives under the plugin dir, which no tenant pod can name.
//!
//! Everything here is Linux; the non-Linux build keeps the signatures so
//! the crate's unit tests run on macOS, and every call reports
//! `Unsupported`.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const FUSE_DEV: &str = "/dev/fuse";
/// What the worker substitutes with `/dev/fd/3` — the mounter's mount
/// point argument in fd mode. Must equal the worker crate's constant.
pub const FUSE_FD_PLACEHOLDER: &str = "{FUSE_FD}";

/// The launch message (the worker crate's `Launch`; JSON is the contract).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Launch {
    pub mode: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub pid: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The `mount(2)` data string for a FUSE mount whose daemon is another
/// process: the AWS Mountpoint CSI v2 / gcsfuse option set verbatim.
/// `user_id`/`group_id` name the daemon's owner for the kernel's
/// `allow_other` check; `default_permissions` makes the kernel enforce
/// the modes the daemon reports; `rootmode` is a directory.
pub fn mount_data(fd: RawFd, owner: (u32, u32)) -> String {
    format!(
        "fd={fd},rootmode=40000,user_id={},group_id={},default_permissions,allow_other",
        owner.0, owner.1
    )
}

/// Frame a launch for the socket: 4-byte big-endian length + JSON.
pub fn frame(launch: &Launch) -> Vec<u8> {
    let body = serde_json::to_vec(launch).expect("launch serializes");
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

fn read_reply(stream: &mut UnixStream, deadline: Duration) -> io::Result<Reply> {
    stream.set_read_timeout(Some(deadline))?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n > 1 << 16 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("reply of {n} bytes")));
    }
    let mut body = vec![0u8; n];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Connect to the worker's socket and send the launch, with the fd if
/// given. Returns the worker's answer.
pub fn send_launch(sock: &Path, launch: &Launch, fd: Option<RawFd>, reply_deadline: Duration) -> io::Result<Reply> {
    let mut stream = UnixStream::connect(sock)?;
    let framed = frame(launch);
    #[cfg(target_os = "linux")]
    {
        use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
        use std::io::IoSlice;
        let fds = fd.map(|f| [f]);
        let cmsgs: Vec<ControlMessage> = match fds.as_ref() {
            Some(f) => vec![ControlMessage::ScmRights(f)],
            None => vec![],
        };
        let sent = sendmsg::<()>(stream.as_raw_fd(), &[IoSlice::new(&framed)], &cmsgs, MsgFlags::empty(), None)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("sendmsg: {e}")))?;
        if sent < framed.len() {
            stream.write_all(&framed[sent..])?;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = fd;
        stream.write_all(&framed)?;
    }
    stream.flush()?;
    read_reply(&mut stream, reply_deadline)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    use std::os::fd::FromRawFd;

    pub fn open_and_mount(src: &Path, owner: (u32, u32), read_only: bool, fsname: &str) -> io::Result<OwnedFd> {
        std::fs::create_dir_all(src)?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(FUSE_DEV)?;
        // SAFETY: we own the File; take its fd as an OwnedFd.
        let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(std::os::fd::IntoRawFd::into_raw_fd(file)) };
        let data = mount_data(fd.as_raw_fd(), owner);
        let mut flags = MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOATIME;
        if read_only {
            flags |= MsFlags::MS_RDONLY;
        }
        mount(Some(fsname), src, Some("fuse"), flags, Some(data.as_str()))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("mount(fuse) at {}: {e}", src.display())))?;
        Ok(fd)
    }

    pub fn bind_mount(src: &Path, target: &Path, read_only: bool) -> io::Result<()> {
        std::fs::create_dir_all(target)?;
        mount(Some(src), target, None::<&str>, MsFlags::MS_BIND, None::<&str>)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("bind {} → {}: {e}", src.display(), target.display())))?;
        if read_only {
            mount(
                None::<&str>,
                target,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
                None::<&str>,
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("remount ro {}: {e}", target.display())))?;
        }
        Ok(())
    }

    /// `umount2(MNT_DETACH)`; "not mounted" and "no such path" are success.
    pub fn unmount(path: &Path, detach: bool) -> io::Result<()> {
        let flags = if detach { MntFlags::MNT_DETACH } else { MntFlags::empty() };
        match umount2(path, flags) {
            Ok(()) => Ok(()),
            Err(nix::errno::Errno::EINVAL) | Err(nix::errno::Errno::ENOENT) => Ok(()),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("umount {}: {e}", path.display()))),
        }
    }

    /// Mount-point test against `/proc/self/mountinfo`, NOT device
    /// numbers.
    ///
    /// The device-number test is the classic one and it is WRONG for the
    /// mount this driver makes most often: a `MS_BIND` of a directory
    /// into a target on the SAME filesystem keeps the device number, so
    /// `st_dev` matches its parent's and the test answers "not a mount".
    /// Measured on kind: every lean bind (a plugin-owned tree bound into
    /// the pod, both under /var/lib/kubelet) read as unmounted, so a
    /// republish fell through to the "unfinished publish, start over"
    /// path and DELETED THE TENANT'S TREE under a running pod — the
    /// files an agent had written vanished mid-run and the next publish
    /// captured only what was written after the wipe.
    ///
    /// A dead FUSE mount answers `ENOTCONN` to `stat`; it is still a
    /// mount and is reported as one (the caller must unmount, not stat
    /// again), and mountinfo lists it too.
    pub fn is_mountpoint(path: &Path) -> io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
        let want = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => return Ok(true),
            Err(e) => return Err(e),
        };
        let info = std::fs::read_to_string("/proc/self/mountinfo")?;
        Ok(mountinfo_has(&info, &want.to_string_lossy()))
    }

    /// `statfs` on a FUSE mount blocks until the daemon has answered
    /// INIT, so this is a liveness AND readiness probe. The blocking call
    /// runs on a thread with a short timeout per attempt; the caller's
    /// deadline bounds the whole wait.
    pub async fn wait_ready(src: &Path, deadline: Duration) -> Result<(), String> {
        let start = std::time::Instant::now();
        loop {
            let p = src.to_path_buf();
            let probe = tokio::task::spawn_blocking(move || -> Result<(), String> {
                nix::sys::statfs::statfs(&p).map_err(|e| format!("statfs: {e}"))?;
                // A bounded readdir proves the daemon serves lookups, not
                // just INIT.
                std::fs::read_dir(&p).map_err(|e| format!("readdir: {e}"))?.next();
                Ok(())
            });
            match tokio::time::timeout(Duration::from_secs(3), probe).await {
                Ok(Ok(Ok(()))) => return Ok(()),
                Ok(Ok(Err(e))) if e.contains("ENOTCONN") || e.contains("Transport endpoint") => {
                    return Err(format!("mounter died before serving the mount ({e})"));
                }
                Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {}
            }
            if start.elapsed() > deadline {
                return Err(format!("mount at {} not ready within {}s", src.display(), deadline.as_secs()));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Is `path` a mount point according to a `/proc/self/mountinfo` body?
///
/// Field 5 (1-based) is the mount point, and octal escapes (`\040` for
/// a space) are how the kernel encodes the characters that would break
/// the field split.
pub fn mountinfo_has(info: &str, path: &str) -> bool {
    info.lines().any(|l| {
        l.split(' ').nth(4).map(|m| unescape_mountinfo(m) == path).unwrap_or(false)
    })
}

fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;
    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "FUSE mounting is Linux-only")
    }
    pub fn open_and_mount(_: &Path, _: (u32, u32), _: bool, _: &str) -> io::Result<OwnedFd> {
        Err(unsupported())
    }
    pub fn bind_mount(_: &Path, _: &Path, _: bool) -> io::Result<()> {
        Err(unsupported())
    }
    pub fn unmount(_: &Path, _: bool) -> io::Result<()> {
        Err(unsupported())
    }
    pub fn is_mountpoint(_: &Path) -> io::Result<bool> {
        Err(unsupported())
    }
    pub async fn wait_ready(_: &Path, _: Duration) -> Result<(), String> {
        Err("FUSE mounting is Linux-only".into())
    }
}

pub use imp::{bind_mount, is_mountpoint, open_and_mount, unmount, wait_ready};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_data_is_the_prior_art_option_set() {
        assert_eq!(
            mount_data(7, (1001, 1002)),
            "fd=7,rootmode=40000,user_id=1001,group_id=1002,default_permissions,allow_other"
        );
    }

    #[test]
    fn frame_is_length_prefixed_json() {
        let l = Launch { mode: "lean".into(), args: vec!["run".into()], env: BTreeMap::new() };
        let f = frame(&l);
        let n = u32::from_be_bytes([f[0], f[1], f[2], f[3]]) as usize;
        assert_eq!(n, f.len() - 4);
        assert_eq!(serde_json::from_slice::<Launch>(&f[4..]).unwrap(), l);
    }

    /// The bind this driver makes for a lean volume is a directory bound
    /// into a target on the SAME filesystem: `st_dev` cannot see it, and
    /// mountinfo must.
    #[test]
    fn mountinfo_finds_a_same_filesystem_bind() {
        let info = "\
25 30 0:23 / /var/lib/kubelet rw,relatime shared:15 - ext4 /dev/vda1 rw
900 25 0:23 /plugins/s3.csi.chert.us/volumes/csi-abc/tree /var/lib/kubelet/pods/uid/volumes/kubernetes.io~csi/ws/mount rw,relatime shared:15 - ext4 /dev/vda1 rw
901 25 0:44 / /var/lib/kubelet/plugins/s3.csi.chert.us/volumes/csi-def/src rw,nosuid,nodev,noatime shared:99 - fuse mount-s3 rw,user_id=1001
";
        assert!(mountinfo_has(info, "/var/lib/kubelet/pods/uid/volumes/kubernetes.io~csi/ws/mount"), "a same-filesystem bind IS a mount point");
        assert!(mountinfo_has(info, "/var/lib/kubelet/plugins/s3.csi.chert.us/volumes/csi-def/src"));
        assert!(!mountinfo_has(info, "/var/lib/kubelet/plugins/s3.csi.chert.us/volumes/csi-abc/tree"), "the bind SOURCE is not itself a mount point");
        assert!(!mountinfo_has(info, "/var/lib/kubelet/pods/uid/volumes"));
    }

    /// The kernel octal-escapes what would break the field split.
    #[test]
    fn mountinfo_unescapes_the_mount_point() {
        let info = "30 25 0:23 / /var/lib/kubelet/odd\\040name rw - ext4 /dev/vda1 rw\n";
        assert!(mountinfo_has(info, "/var/lib/kubelet/odd name"));
    }

    /// The reply path against a stand-in worker on a socketpair.
    #[test]
    fn send_launch_round_trips_a_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mount.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            s.read_exact(&mut len).unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            s.read_exact(&mut body).unwrap();
            let l: Launch = serde_json::from_slice(&body).unwrap();
            let reply = serde_json::to_vec(&Reply { ok: true, pid: Some(42), error: None }).unwrap();
            s.write_all(&(reply.len() as u32).to_be_bytes()).unwrap();
            s.write_all(&reply).unwrap();
            l
        });
        let l = Launch { mode: "lean".into(), args: vec!["run".into()], env: BTreeMap::new() };
        let r = send_launch(&sock, &l, None, Duration::from_secs(5)).unwrap();
        assert!(r.ok);
        assert_eq!(r.pid, Some(42));
        assert_eq!(server.join().unwrap(), l);
    }
}
