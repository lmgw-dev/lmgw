//! The processor: its name, how many logical processors there are
//! ([`logical`]), and the instruction sets the engines' CPU builds pick their kernels
//! by (Windows design §4.4, WP6). Read with `cpuid` through std, so it needs
//! no file and no right; on a processor other than x86-64 the name and the
//! instruction sets are not read yet.

/// What [`read`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpu {
    /// `cpuid`'s brand string ("AMD Ryzen 9 7950X 16-Core Processor"); `None`
    /// where it is not read.
    pub brand: Option<String>,
    /// The logical processors ([`logical`]).
    pub logical: usize,
    /// Each instruction set the check names, and whether it is there, in
    /// [`FEATURES`]' order; empty where they are not read.
    pub features: Vec<(&'static str, bool)>,
}

/// The instruction sets named, as `is_x86_feature_detected!` spells them:
/// what ggml's CPU backend variants are built for (SSE4.2 the oldest one,
/// AVX2 with FMA and F16C the common one, AVX-512 the newest).
pub const FEATURES: [&str; 8] = [
    "sse4.2", "avx", "avx2", "fma", "f16c", "avx512f", "avx512bw", "avx512vl",
];

impl Cpu {
    /// Whether instruction set `name` is there; `None` when it is not read.
    pub fn has(&self, name: &str) -> Option<bool> {
        self.features
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, on)| *on)
    }
}

/// This machine's processor.
pub fn read() -> Cpu {
    Cpu {
        brand: brand(),
        logical: logical(),
        features: features(),
    }
}

/// The logical processors. Windows: every processor group's
/// (`GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)`), as a job's CPU rate is
/// a share of all of them; std's count sees one group, at most 64. Elsewhere
/// the ones this process may run on (std's count: affinity and cgroup
/// quota).
pub fn logical() -> usize {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetActiveProcessorCount, ALL_PROCESSOR_GROUPS,
        };
        // SAFETY: a plain query; 0 means it failed.
        let n = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
        if n > 0 {
            return n as usize;
        }
    }
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

#[cfg(target_arch = "x86_64")]
fn features() -> Vec<(&'static str, bool)> {
    FEATURES
        .iter()
        .map(|&f| {
            let on = match f {
                "sse4.2" => std::arch::is_x86_feature_detected!("sse4.2"),
                "avx" => std::arch::is_x86_feature_detected!("avx"),
                "avx2" => std::arch::is_x86_feature_detected!("avx2"),
                "fma" => std::arch::is_x86_feature_detected!("fma"),
                "f16c" => std::arch::is_x86_feature_detected!("f16c"),
                "avx512f" => std::arch::is_x86_feature_detected!("avx512f"),
                "avx512bw" => std::arch::is_x86_feature_detected!("avx512bw"),
                "avx512vl" => std::arch::is_x86_feature_detected!("avx512vl"),
                _ => false,
            };
            (f, on)
        })
        .collect()
}

#[cfg(not(target_arch = "x86_64"))]
fn features() -> Vec<(&'static str, bool)> {
    Vec::new()
}

/// `cpuid` leaves 0x80000002 to 0x80000004: 48 bytes of text.
#[cfg(target_arch = "x86_64")]
fn brand() -> Option<String> {
    use std::arch::x86_64::__cpuid;
    // `cpuid` is in every x86-64 processor; leaf 0x80000000 says how far the
    // extended leaves go.
    let max = __cpuid(0x8000_0000).eax;
    if max < 0x8000_0004 {
        return None;
    }
    let mut bytes = Vec::with_capacity(48);
    for leaf in 0x8000_0002..=0x8000_0004u32 {
        let r = __cpuid(leaf);
        for reg in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&reg.to_le_bytes());
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_matches(char::from(0)).trim();
    (!text.is_empty()).then(|| text.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[cfg(not(target_arch = "x86_64"))]
fn brand() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What any x86-64 machine answers: a name, a count, and an answer for
    /// every set (which ones it has depends on the machine, and x86-64's
    /// baseline is SSE2, so none is asserted).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn this_processor_has_a_name_a_count_and_an_answer_for_every_set() {
        let cpu = read();
        assert!(cpu.brand.as_deref().is_some_and(|b| !b.is_empty()));
        assert!(cpu.logical >= 1);
        assert_eq!(cpu.features.len(), FEATURES.len());
        for f in FEATURES {
            assert!(cpu.has(f).is_some(), "{f}");
        }
        println!("{cpu:?}");
    }
}
