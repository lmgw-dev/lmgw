//! §1.5 in the server image: each place by the volume it lies on, as the
//! container's mount table shows it. A path under `/data` is not on the
//! data volume when the unit mounts something else there (a host folder of
//! models at `/data/models`), so the path alone does not say.

use std::path::Path;

use lmgw_machine::Facts;

use super::VOLUMES;

/// The mount table the container sees.
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// The container's mount points, as [`MOUNTINFO`] lists them, or why they
/// could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::machine) struct Mounts(Result<Vec<String>, String>);

/// The mount points of the process the check runs in.
pub(in crate::machine) fn read_mounts(f: &dyn Facts) -> Mounts {
    Mounts(
        lmgw_machine::facts::read_text(f, Path::new(MOUNTINFO))
            .map_err(|e| format!("{MOUNTINFO} could not be read: {e}"))
            .map(|t| parse(&t)),
    )
}

/// Each line's mount point (its fifth field), with the kernel's octal
/// escapes (`\040` for a space) undone.
fn parse(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(unescape)
        .collect()
}

fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)))
            .and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok());
        match (b[i], octal) {
            (b'\\', Some(c)) => {
                out.push(c);
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `path` is `mount` or lies under it.
fn under(path: &str, mount: &str) -> bool {
    mount == "/"
        || path == mount
        || path
            .strip_prefix(mount)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `path` with what it lies on (§3.2): the image's volume when the deepest
/// mount that holds it is one; otherwise the mount it is on, or the
/// container's own file system, whose contents go when the container is
/// recreated.
pub(in crate::machine) fn on_volume(path: &str, mounts: &Mounts) -> String {
    let mounts = match &mounts.0 {
        Ok(m) => m,
        Err(e) => return format!("{path} (which mount it lies on is not known: {e})"),
    };
    let deepest = mounts
        .iter()
        .filter(|m| under(path, m))
        .max_by_key(|m| m.len());
    match deepest.map(String::as_str) {
        Some(m) if VOLUMES.contains(&m) => format!("{path} (volume {m})"),
        Some(m) if m != "/" => format!(
            "{path} (on the mount at {m}, none of the image's volumes {})",
            VOLUMES.join(", ")
        ),
        _ => format!(
            "{path} (on none of the image's volumes {} and on no mount: what is kept there goes \
             when the container is recreated)",
            VOLUMES.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The image's mounts as Podman sets them up, with a host folder of
    /// models over `/data/models`.
    const TABLE: &str = "\
1 0 0:50 / / rw,relatime - overlay overlay rw
2 1 0:51 / /proc rw - proc proc rw
3 1 0:52 /volumes/lmgw-data/_data /data rw - btrfs /dev/nvme0n1p3 rw
4 1 0:52 /volumes/lmgw-storage/_data /var/lib/containers rw - btrfs /dev/nvme0n1p3 rw
5 1 0:52 /volumes/lmgw-tmp/_data /var/tmp rw - btrfs /dev/nvme0n1p3 rw
6 3 0:53 /srv/models /data/models rw - xfs /dev/sdb1 rw
7 1 0:54 /home/a\\040b /mnt/a\\040b rw - btrfs /dev/nvme0n1p3 rw
";

    #[test]
    fn each_place_is_named_with_the_volume_it_lies_on() {
        let m = Mounts(Ok(parse(TABLE)));
        assert_eq!(
            on_volume("/data/backups", &m),
            "/data/backups (volume /data)"
        );
        assert_eq!(
            on_volume("/var/lib/containers/storage", &m),
            "/var/lib/containers/storage (volume /var/lib/containers)"
        );
        assert_eq!(on_volume("/var/tmp", &m), "/var/tmp (volume /var/tmp)");
        assert_eq!(
            on_volume("/data/models/chat", &m),
            "/data/models/chat (on the mount at /data/models, none of the image's volumes /data, \
             /var/lib/containers, /var/tmp)",
            "a bind mount under /data is not the data volume"
        );
        assert!(on_volume("/mnt/a b/x", &m).contains("on the mount at /mnt/a b,"));
        assert!(on_volume("/database", &m).contains("goes when the container is recreated"));
        assert!(
            on_volume("/data2", &m).contains("on no mount"),
            "/data2 is not under /data"
        );
        let unread = Mounts(Err("no".into()));
        assert_eq!(
            on_volume("/data/models", &unread),
            "/data/models (which mount it lies on is not known: no)"
        );
    }
}
