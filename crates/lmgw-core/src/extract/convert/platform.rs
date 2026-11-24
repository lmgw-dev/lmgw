//! Which install hints this system gets (document-conversion design §5.1):
//! the server image in container mode, otherwise this build's system, a
//! Linux told apart by its `/etc/os-release`: only a Fedora-like one gets
//! the `sudo dnf install` line.

use lmgw_api_types::documents::InstallPlatform;

/// Where Linux distributions say what they are (`os-release(5)`).
const OS_RELEASE: &str = "/etc/os-release";

/// `ID` and `ID_LIKE` values that mean `dnf` and Fedora's package names.
const FEDORA_LIKE: &[&str] = &[
    "fedora",
    "rhel",
    "centos",
    "rocky",
    "almalinux",
    "nobara",
    "amzn",
];

/// The install hints' platform: the server image in container mode,
/// otherwise this build's system.
pub fn platform(container: bool) -> InstallPlatform {
    if container {
        InstallPlatform::ServerImage
    } else if cfg!(windows) {
        InstallPlatform::Windows
    } else if cfg!(target_os = "macos") {
        InstallPlatform::Macos
    } else {
        linux_platform(std::fs::read_to_string(OS_RELEASE).ok().as_deref())
    }
}

/// A Linux by its `os-release` text (`None`: the file is missing, which
/// counts as unknown).
pub fn linux_platform(os_release: Option<&str>) -> InstallPlatform {
    let like = os_release.is_some_and(|text| {
        text.lines()
            .filter_map(|l| {
                let (key, value) = l.trim().split_once('=')?;
                matches!(key, "ID" | "ID_LIKE").then_some(value)
            })
            .flat_map(|v| {
                v.trim_matches(|c| c == '"' || c == '\'')
                    .split_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect::<Vec<_>>()
            })
            .any(|id| FEDORA_LIKE.contains(&id.as_str()))
    });
    if like {
        InstallPlatform::Fedora
    } else {
        InstallPlatform::OtherLinux
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fields of the real files, trimmed to the lines that matter.
    const FEDORA: &str = "NAME=\"Fedora Linux\"\nVERSION_ID=45\nID=fedora\n";
    const RHEL_CLONE: &str = "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n";
    const NOBARA_LIKE: &str = "ID=somerespin\nID_LIKE=\"rhel fedora\"\n";
    const DEBIAN: &str = "PRETTY_NAME=\"Debian GNU/Linux 13\"\nID=debian\n";
    const UBUNTU: &str = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";
    const ARCH: &str = "ID=arch\nBUILD_ID=rolling\n";

    #[test]
    fn fedora_like_systems_get_the_dnf_line() {
        for text in [FEDORA, RHEL_CLONE, NOBARA_LIKE] {
            assert_eq!(
                linux_platform(Some(text)),
                InstallPlatform::Fedora,
                "{text}"
            );
        }
    }

    #[test]
    fn other_systems_and_a_missing_file_get_no_dnf_line() {
        for text in [DEBIAN, UBUNTU, ARCH, "", "ID_LIKE=fedoraish\n"] {
            assert_eq!(
                linux_platform(Some(text)),
                InstallPlatform::OtherLinux,
                "{text}"
            );
        }
        assert_eq!(linux_platform(None), InstallPlatform::OtherLinux);
    }
}
