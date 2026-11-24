//! What each Landlock ABI can enforce (security round B, M3): the rights
//! a ruleset of `lmgw-doc-reader run` handles for the ABI the kernel
//! reports ([`ruleset`]), and the protections a kernel of an older ABI does
//! not have ([`lacking`]), which the gateway names on Settings › Documents
//! and in its log. Pure numbers (`linux/landlock.h`), so the rule building
//! of every ABI is tested on any kernel.

// The filesystem rights.
pub const FS_EXECUTE: u64 = 1 << 0;
pub const FS_WRITE_FILE: u64 = 1 << 1;
pub const FS_READ_FILE: u64 = 1 << 2;
pub const FS_READ_DIR: u64 = 1 << 3;
pub const FS_REMOVE_DIR: u64 = 1 << 4;
pub const FS_REMOVE_FILE: u64 = 1 << 5;
pub const FS_MAKE_CHAR: u64 = 1 << 6;
pub const FS_MAKE_DIR: u64 = 1 << 7;
pub const FS_MAKE_REG: u64 = 1 << 8;
pub const FS_MAKE_SOCK: u64 = 1 << 9;
pub const FS_MAKE_FIFO: u64 = 1 << 10;
pub const FS_MAKE_BLOCK: u64 = 1 << 11;
pub const FS_MAKE_SYM: u64 = 1 << 12;
pub const FS_REFER: u64 = 1 << 13;
pub const FS_TRUNCATE: u64 = 1 << 14;
pub const FS_IOCTL_DEV: u64 = 1 << 15;
pub const FS_RESOLVE_UNIX: u64 = 1 << 16;
/// ABI 1's thirteen rights, execute to make_sym.
pub const FS_ABI1: u64 = (1 << 13) - 1;
/// The rights a rule on a file (not a directory) may name.
pub const FS_FILE: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

/// Reading files and listing directories.
pub const READ: u64 = FS_READ_FILE | FS_READ_DIR;

/// What a `--read-write` rule allows besides [`READ`]: write, create and
/// remove files and directories. No symbolic link and no link or rename
/// into another directory (`refer`): a tool cannot leave a link in its run
/// directory that the gateway, reading its output, would follow
/// (security review L3).
pub const WRITE: u64 = FS_WRITE_FILE
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_TRUNCATE
    | FS_IOCTL_DEV;

pub const NET_BIND_TCP: u64 = 1 << 0;
pub const NET_CONNECT_TCP: u64 = 1 << 1;
pub const NET_BIND_UDP: u64 = 1 << 2;
pub const NET_CONNECT_SEND_UDP: u64 = 1 << 3;

pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
pub const SCOPE_SIGNAL: u64 = 1 << 1;

/// The first ABI that denies truncating a file (Linux 6.2).
pub const TRUNCATE_ABI: u32 = 3;
/// The first ABI that denies TCP bind and connect (Linux 6.7).
pub const TCP_ABI: u32 = 4;
/// The first ABI that limits `ioctl` on devices (Linux 6.10).
pub const IOCTL_DEV_ABI: u32 = 5;
/// The first ABI that scopes abstract Unix sockets and signals to the
/// confined processes (Linux 6.12).
pub const SCOPES_ABI: u32 = 6;
/// The first ABI that handles connecting to a pathname Unix socket.
pub const UNIX_ABI: u32 = 9;
/// The first ABI that denies UDP bind and send.
pub const UDP_ABI: u32 = 10;

/// The filesystem rights `abi` knows.
pub fn fs_rights(abi: u32) -> u64 {
    let mut r = FS_ABI1;
    if abi >= 2 {
        r |= FS_REFER;
    }
    if abi >= TRUNCATE_ABI {
        r |= FS_TRUNCATE;
    }
    if abi >= IOCTL_DEV_ABI {
        r |= FS_IOCTL_DEV;
    }
    if abi >= UNIX_ABI {
        r |= FS_RESOLVE_UNIX;
    }
    r
}

/// The network rights `abi` knows.
pub fn net_rights(abi: u32) -> u64 {
    let mut r = 0;
    if abi >= TCP_ABI {
        r |= NET_BIND_TCP | NET_CONNECT_TCP;
    }
    if abi >= UDP_ABI {
        r |= NET_BIND_UDP | NET_CONNECT_SEND_UDP;
    }
    r
}

/// The scopes `abi` knows.
pub fn scopes(abi: u32) -> u64 {
    if abi >= SCOPES_ABI {
        SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
    } else {
        0
    }
}

/// A ruleset's attribute for one ABI: what it handles, and how many bytes
/// of `struct landlock_ruleset_attr` that ABI knows (the kernel refuses a
/// larger struct whose extra fields are not zero only where it knows them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ruleset {
    pub handled_fs: u64,
    pub handled_net: u64,
    pub scoped: u64,
    pub size: usize,
}

/// The ruleset `lmgw-doc-reader run` makes on a kernel of `abi`: every
/// filesystem right it knows, its network rights when `deny_net`, and its
/// scopes.
pub fn ruleset(abi: u32, deny_net: bool) -> Ruleset {
    Ruleset {
        handled_fs: fs_rights(abi),
        handled_net: if deny_net { net_rights(abi) } else { 0 },
        scoped: scopes(abi),
        size: match abi {
            1..=3 => 8,
            4 | 5 => 16,
            _ => 24,
        },
    }
}

/// What a rule of a class allows on a kernel of `abi`: the class's rights
/// that the ABI handles (a right it does not handle is allowed anyway).
pub fn allowed(abi: u32, class: u64) -> u64 {
    class & fs_rights(abi)
}

/// One protection a kernel of an older ABI does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lack {
    /// What stays open, in plain words.
    pub what: &'static str,
    /// The first ABI that has it.
    pub from: u32,
}

/// Every protection of lmgw's rules that a kernel of `abi` lacks, oldest
/// first: truncating files below ABI 3 (a tool running hostile code could
/// empty any file the gateway's user may write, lmgw's database included:
/// the kernel does not check `truncate` against the rules before ABI 3),
/// the network over TCP below 4, `ioctl` on devices below 5, abstract Unix
/// sockets and signals below 6, pathname Unix sockets below 9, UDP below
/// 10.
pub fn lacking(abi: u32) -> Vec<Lack> {
    [
        Lack {
            what: "emptying files by truncating them is not blocked, so a tool running hostile \
                   code could empty any file this user may write, lmgw's database included",
            from: TRUNCATE_ABI,
        },
        Lack {
            what: "TCP connections are not blocked",
            from: TCP_ABI,
        },
        Lack {
            what: "ioctl calls on devices are not limited",
            from: IOCTL_DEV_ABI,
        },
        Lack {
            what: "abstract Unix sockets and signals to other processes are not blocked",
            from: SCOPES_ABI,
        },
        Lack {
            what: "connecting to Unix sockets by path is not blocked",
            from: UNIX_ABI,
        },
        Lack {
            what: "UDP is not blocked",
            from: UDP_ABI,
        },
    ]
    .into_iter()
    .filter(|l| abi < l.from)
    .collect()
}

/// Each of [`lacking`]'s protections said with the ABI found: empty when
/// the kernel has them all.
pub fn lacking_said(abi: u32) -> Vec<String> {
    lacking(abi)
        .into_iter()
        .map(|l| {
            format!(
                "{} (that needs Landlock ABI {}; this kernel has {abi})",
                l.what, l.from
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_abi_handles_what_it_knows_and_nothing_more() {
        let r1 = ruleset(1, true);
        assert_eq!(r1.handled_fs, FS_ABI1);
        assert_eq!((r1.handled_net, r1.scoped, r1.size), (0, 0, 8));
        assert_eq!(ruleset(2, true).handled_fs & FS_REFER, FS_REFER);
        assert_eq!(ruleset(3, true).handled_fs & FS_TRUNCATE, FS_TRUNCATE);
        let r4 = ruleset(4, true);
        assert_eq!(r4.handled_net, NET_BIND_TCP | NET_CONNECT_TCP);
        assert_eq!((r4.scoped, r4.size), (0, 16));
        assert_eq!(ruleset(4, false).handled_net, 0);
        assert_eq!(ruleset(5, true).handled_fs & FS_IOCTL_DEV, FS_IOCTL_DEV);
        let r6 = ruleset(6, true);
        assert_eq!(r6.scoped, SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL);
        assert_eq!(r6.size, 24);
        assert_eq!(ruleset(8, true).handled_fs & FS_RESOLVE_UNIX, 0);
        assert_eq!(
            ruleset(9, true).handled_fs & FS_RESOLVE_UNIX,
            FS_RESOLVE_UNIX
        );
        assert_eq!(ruleset(9, true).handled_net, NET_BIND_TCP | NET_CONNECT_TCP);
        assert_eq!(
            ruleset(10, true).handled_net,
            NET_BIND_TCP | NET_CONNECT_TCP | NET_BIND_UDP | NET_CONNECT_SEND_UDP
        );
        assert_eq!(ruleset(11, true), ruleset(10, true));
    }

    #[test]
    fn a_write_rule_makes_no_link_on_any_abi() {
        for abi in 1..=11 {
            let w = allowed(abi, READ | WRITE);
            assert_eq!(w & (FS_MAKE_SYM | FS_REFER | FS_EXECUTE), 0, "ABI {abi}");
            assert_ne!(w & FS_MAKE_REG, 0, "ABI {abi}");
        }
        // A right the ABI handles but a class leaves out stays denied.
        assert_ne!(fs_rights(1) & FS_MAKE_SYM, 0);
        assert_ne!(fs_rights(2) & FS_REFER, 0);
    }

    #[test]
    fn an_older_kernel_is_told_what_it_lacks_with_the_abi_found() {
        assert!(lacking(11).is_empty() && lacking(10).is_empty());
        let froms = |abi| lacking(abi).iter().map(|l| l.from).collect::<Vec<_>>();
        assert_eq!(froms(1), [3, 4, 5, 6, 9, 10]);
        assert_eq!(froms(2), [3, 4, 5, 6, 9, 10]);
        assert_eq!(froms(3), [4, 5, 6, 9, 10]);
        assert_eq!(froms(4), [5, 6, 9, 10]);
        assert_eq!(froms(5), [6, 9, 10]);
        assert_eq!(froms(6), [9, 10]);
        assert_eq!(froms(9), [10]);
        let said = lacking_said(3);
        assert_eq!(
            said[0],
            "TCP connections are not blocked (that needs Landlock ABI 4; this kernel has 3)"
        );
        assert_eq!(said.len(), 5);
        // The most destructive gap is said first, in plain words, with what
        // it means (security round B review).
        let said = lacking_said(2);
        assert!(
            said[0].starts_with("emptying files by truncating them is not blocked")
                && said[0].contains("lmgw's database")
                && said[0].ends_with("(that needs Landlock ABI 3; this kernel has 2)"),
            "{}",
            said[0]
        );
        assert!(said[2].starts_with("ioctl calls on devices"), "{}", said[2]);
    }
}
