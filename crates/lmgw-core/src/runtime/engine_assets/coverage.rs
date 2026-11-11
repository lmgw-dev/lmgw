//! Whether a pinned CUDA build runs on a card (Windows design §4.2), from
//! its recorded coverage ([`super::pins::CudaPin`]).
//!
//! - **SASS** `sm_XY` runs on a card of compute capability X.Z with Z ≥ Y
//!   (binary compatibility within a major version); an arch-specific
//!   `sm_XYa` runs on X.Y alone.
//! - **PTX** `compute_XY` is JIT-compiled by the driver for any card of X.Y
//!   or newer (`compute_XYa` for X.Y alone), when the driver knows the
//!   PTX's ISA: its CUDA version is at least the build's toolkit. That
//!   first start compiles for minutes on an older card (§2 point 3).
//! - **The driver** runs a build of CUDA major M when its own CUDA version
//!   is M.0 or newer (minor version compatibility).
//!
//! Every file of the build that carries device code has to cover the card.

use super::pins::CudaPin;

/// A CUDA version or a compute capability, `major.minor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// `"12.4"` → 12.4; `"12"` → 12.0.
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.trim().splitn(2, '.');
        let major = it.next()?.parse().ok()?;
        let minor = match it.next() {
            Some(m) => m.parse().ok()?,
            None => 0,
        };
        Some(Self { major, minor })
    }

    /// NVML's `nvmlSystemGetCudaDriverVersion` (13020 for 13.2).
    pub fn from_nvml(v: i32) -> Option<Self> {
        (v > 0).then(|| Self::new(v as u32 / 1000, (v as u32 % 1000) / 10))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// One recorded architecture: `86` or `120a` (or `100f`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arch {
    cc: Version,
    /// `a`: this exact compute capability only.
    exact: bool,
}

fn arch(s: &str) -> Option<Arch> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    let suffix = &s[digits.len()..];
    if digits.len() < 2 {
        return None;
    }
    let (major, minor) = digits.split_at(digits.len() - 1);
    Some(Arch {
        cc: Version::new(major.parse().ok()?, minor.parse().ok()?),
        exact: suffix == "a",
    })
}

/// How a build reaches a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Compiled kernels for it (`sm_86`).
    Sass(String),
    /// Only through the driver's JIT from `compute_<n>`: the first start
    /// compiles for minutes.
    Jit(String),
    /// Not at all, with the sentence that says why.
    No(String),
}

impl Reach {
    pub fn runs(&self) -> bool {
        !matches!(self, Self::No(_))
    }
}

/// Whether `cuda` (a build of CUDA `major`) runs on a card of compute
/// capability `cc` under a driver of CUDA version `driver`.
pub fn reach(cuda: &CudaPin, major: u32, cc: Version, driver: Version) -> Reach {
    if driver.major < major {
        return Reach::No(format!(
            "the driver supports CUDA {driver}, and this build needs CUDA {major}"
        ));
    }
    let toolkit = Version::parse(&cuda.toolkit).unwrap_or(Version::new(major, 0));
    let mut weakest: Option<Reach> = None;
    for f in &cuda.files {
        let sass = f
            .sass
            .iter()
            .filter_map(|s| Some((s, arch(s)?)))
            .filter(|(_, a)| {
                if a.exact {
                    a.cc == cc
                } else {
                    a.cc.major == cc.major && a.cc.minor <= cc.minor
                }
            })
            .map(|(s, _)| s)
            .next_back();
        if let Some(s) = sass {
            if weakest.is_none() {
                weakest = Some(Reach::Sass(format!("sm_{s}")));
            }
            continue;
        }
        let ptx = f
            .ptx
            .iter()
            .filter_map(|s| Some((s, arch(s)?)))
            .filter(|(_, a)| if a.exact { a.cc == cc } else { a.cc <= cc })
            .map(|(s, _)| s)
            .next_back();
        match ptx {
            Some(p) if driver >= toolkit => weakest = Some(Reach::Jit(format!("compute_{p}"))),
            Some(p) => {
                return Reach::No(format!(
                    "{} reaches compute capability {cc} only as compute_{p} PTX of CUDA {toolkit}, \
                     which the driver (CUDA {driver}) cannot compile",
                    f.file
                ))
            }
            None => {
                return Reach::No(format!(
                    "{} has no kernels for compute capability {cc} (SASS {}; PTX {})",
                    f.file,
                    list(&f.sass),
                    list(&f.ptx)
                ))
            }
        }
    }
    weakest.unwrap_or_else(|| Reach::No("the build records no file with device code".into()))
}

fn list(archs: &[String]) -> String {
    if archs.is_empty() {
        "none".into()
    } else {
        archs.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::engine_assets::pins::{self, Variant};

    fn windows() -> &'static pins::PlatformPins {
        pins::table().get("windows-x86_64").unwrap()
    }

    fn cuda(engine: &str, v: Variant) -> &'static CudaPin {
        windows()
            .engine(engine)
            .unwrap()
            .build(v)
            .unwrap()
            .cuda
            .as_ref()
            .unwrap()
    }

    /// A GTX 970 test card (cc 5.2, a driver with CUDA 12.6): llama.cpp's
    /// CUDA 12 build reaches it through compute_50 PTX; sd.cpp's and
    /// audio.cpp's do not (§2's "no kernel image").
    #[test]
    fn the_recorded_coverage_matches_the_spike_on_a_gtx_970() {
        let maxwell = Version::new(5, 2);
        let driver = Version::new(12, 6);
        assert_eq!(
            reach(cuda("llama", Variant::Cuda12), 12, maxwell, driver),
            Reach::Jit("compute_50".into())
        );
        assert!(!reach(cuda("sdcpp", Variant::Cuda12), 12, maxwell, driver).runs());
        assert!(!reach(cuda("audio", Variant::Cuda12), 12, maxwell, driver).runs());
        let r = reach(cuda("llama", Variant::Cuda13), 13, maxwell, driver);
        assert!(
            matches!(&r, Reach::No(m) if m.contains("CUDA 12.6")),
            "{r:?}"
        );
    }

    #[test]
    fn sass_is_forward_compatible_within_a_major_and_a_is_exact() {
        let c = CudaPin {
            toolkit: "13.5".into(),
            files: vec![pins::CudaFile {
                file: "ggml-cuda.dll".into(),
                sass: vec!["86".into(), "120a".into()],
                ptx: vec!["75".into()],
                ptx_isa: Some("9.5".into()),
            }],
        };
        let d = Version::new(13, 5);
        assert_eq!(
            reach(&c, 13, Version::new(8, 9), d),
            Reach::Sass("sm_86".into())
        );
        assert_eq!(
            reach(&c, 13, Version::new(12, 0), d),
            Reach::Sass("sm_120a".into())
        );
        // 12.1 is not 12.0's arch-specific code: the PTX of 7.5 is JIT-compiled.
        assert_eq!(
            reach(&c, 13, Version::new(12, 1), d),
            Reach::Jit("compute_75".into())
        );
        // A driver older than the PTX's toolkit cannot compile it.
        assert!(!reach(&c, 13, Version::new(12, 1), Version::new(13, 0)).runs());
        assert!(!reach(&c, 13, Version::new(7, 0), d).runs());
    }

    #[test]
    fn versions_parse() {
        assert_eq!(Version::parse("12.4"), Some(Version::new(12, 4)));
        assert_eq!(Version::parse("12"), Some(Version::new(12, 0)));
        assert_eq!(Version::from_nvml(12060), Some(Version::new(12, 6)));
        assert_eq!(Version::from_nvml(13020), Some(Version::new(13, 2)));
    }
}
