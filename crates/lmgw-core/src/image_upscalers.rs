//! The image class's upscalers (chat-media-tools design §2, §9).
//!
//! sd-server's `POST /sdcpp/v1/upscale` runs an ESRGAN model it finds at the
//! **top level** of `--hires-upscalers-dir`, which lmgw renders as
//! `/models/upscalers` on every image row (`runtime::image::DEFAULT_DIRS`):
//! one directory, `<image models_dir>/upscalers`, serves every row. A file
//! there is named by its stem, by sd-server and by lmgw alike — the
//! `upscaler` enum of `image__upscale` and the card's Upscale are the stems
//! [`on_disk`] lists. A row whose `files` point its upscalers directory
//! elsewhere reads that directory instead; the enum lists the default one.
//!
//! The downloader's `<owner>/<repo>/<file>` layout does not put a file
//! where sd-server scans, so the shipped entry downloads into
//! `upscalers/<file>` ([`add`]).
//!
//! # The shipped entry
//!
//! One model, read from the hub's public metadata API on **2026-10-21**
//! (`scripts/hf-recipe-check.py Comfy-Org/Real-ESRGAN_repackaged`: one
//! weight file, its exact size, `gated=False`; the repo's card states
//! `license: bsd-3-clause`). Real-ESRGAN x4plus is the general-purpose 4×
//! model of the Real-ESRGAN project; Comfy-Org's repackage is its
//! `params_ema` weights as safetensors (keys `conv_first`, `body.N.rdbM`,
//! `conv_up1/2`, read from the file's header), the RRDBNet layout
//! sd-server detects. Nothing here is from memory.

use std::path::Path;

use lmgw_api_types as dto;

use crate::hf;

mod add;

pub use add::image_upscaler_add;

/// The upscalers directory, relative to the image models dir — the class
/// default of `hires_upscalers_dir` (`runtime::image::DEFAULT_DIRS`).
pub const DIR: &str = "upscalers";

/// One shipped upscaler.
#[derive(Debug, Clone, Copy)]
pub struct ShippedUpscaler {
    /// Stable id — what `image_upscaler_add` takes. Never renamed.
    pub key: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub size_bytes: u64,
    pub gated: bool,
    pub licence: &'static str,
    /// How many times larger a side gets per run.
    pub scale: u32,
}

impl ShippedUpscaler {
    /// Where the file lands, relative to the image models dir.
    pub fn dest_rel_path(&self) -> String {
        format!("{DIR}/{}", self.file)
    }

    /// The name sd-server and the tools call it by.
    pub fn stem(&self) -> &'static str {
        stem_of(self.file)
    }
}

/// The shipped list (module doc).
pub const SHIPPED: &[ShippedUpscaler] = &[ShippedUpscaler {
    key: "realesrgan-x4plus",
    display_name: "Real-ESRGAN x4plus",
    description: "The general-purpose 4x upscaler of the Real-ESRGAN project: photos and \
                  renders alike, one side four times larger per run.",
    repo: "Comfy-Org/Real-ESRGAN_repackaged",
    file: "RealESRGAN_x4plus.safetensors",
    size_bytes: 66_857_836,
    gated: false,
    licence: "BSD-3-Clause",
    scale: 4,
}];

/// The shipped entry `key`.
pub fn find(key: &str) -> Option<&'static ShippedUpscaler> {
    SHIPPED.iter().find(|u| u.key == key.trim())
}

/// How many times larger upscaler `name` (a stem) makes a side per run:
/// the shipped entry's own figure, else what the name says — `x4` or `4x`
/// as a word of it (`RealESRGAN_x4plus`, `4x-UltraSharp`); `None` when it
/// says nothing (sd-server reads the scale from the weights, and lmgw does
/// not open them).
pub(crate) fn scale_of(name: &str) -> Option<u32> {
    if let Some(u) = SHIPPED.iter().find(|u| u.stem() == name) {
        return Some(u.scale);
    }
    name.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find_map(|word| {
            let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
            let n = match word.strip_prefix('x') {
                Some(rest) => &rest[..digits(rest)],
                None => {
                    let d = digits(word);
                    word[d..].starts_with('x').then(|| &word[..d])?
                }
            };
            n.parse::<u32>().ok().filter(|n| (1..=16).contains(n))
        })
}

/// A file name without its extension.
fn stem_of(file: &str) -> &str {
    Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file)
}

/// The upscaler files at the top level of `<models_dir>/upscalers` (what
/// sd-server scans): weight files only, a download's `.part` left out,
/// sorted by name. Empty for an unconfigured models dir or a directory that
/// is not there.
pub fn on_disk(models_dir: &str) -> Vec<dto::ImageUpscalerFile> {
    if models_dir.trim().is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(Path::new(models_dir).join(DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<dto::ImageUpscalerFile> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(std::fs::Metadata::is_file)?;
            let file = e.file_name().to_str()?.to_string();
            hf::accepts_file("image", &file).then(|| dto::ImageUpscalerFile {
                name: stem_of(&file).to_string(),
                file,
                size_bytes: meta.len(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The names [`on_disk`] lists: the `upscaler` enum.
pub fn names(models_dir: &str) -> Vec<String> {
    on_disk(models_dir).into_iter().map(|f| f.name).collect()
}

/// The upscaler half of `image_recipes`: the shipped entries joined
/// against the disk and the tracked downloads, and every file on disk.
pub fn listing(models_dir: &str, tracked: &[crate::store::HfModelRow]) -> dto::ImageUpscalers {
    let shipped = SHIPPED
        .iter()
        .map(|u| {
            let dest = u.dest_rel_path();
            let row = tracked
                .iter()
                .find(|t| t.repo == u.repo && t.file == u.file && t.dest_path == dest);
            dto::ImageUpscalerEntry {
                key: u.key.to_string(),
                display_name: u.display_name.to_string(),
                description: u.description.to_string(),
                repo: u.repo.to_string(),
                file: u.file.to_string(),
                present: present(models_dir, u),
                dest_path: dest,
                size_bytes: u.size_bytes,
                size: hf::fmt_bytes(u.size_bytes),
                gated: u.gated,
                licence: u.licence.to_string(),
                scale: u.scale,
                downloading: row
                    .is_some_and(|t| matches!(t.status.as_str(), "queued" | "downloading")),
                done: row.is_some_and(|t| t.status == "done"),
                download_id: row.map(|t| t.id),
            }
        })
        .collect();
    dto::ImageUpscalers {
        shipped,
        on_disk: on_disk(models_dir),
        dir: DIR.to_string(),
    }
}

/// Whether the shipped file is already where sd-server scans.
fn present(models_dir: &str, u: &ShippedUpscaler) -> bool {
    !models_dir.trim().is_empty() && Path::new(models_dir).join(u.dest_rel_path()).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scale_is_the_shipped_figure_or_the_names_word() {
        assert_eq!(scale_of("RealESRGAN_x4plus"), Some(4));
        assert_eq!(scale_of("4x-UltraSharp"), Some(4));
        assert_eq!(scale_of("4xNomos8kSC"), Some(4));
        assert_eq!(scale_of("RealESRGAN_x2plus"), Some(2));
        assert_eq!(scale_of("x8_esrgan"), Some(8));
        assert_eq!(
            scale_of("ESRGAN_SRx4_DF2KOST"),
            None,
            "srx4 is not a word of its own"
        );
        assert_eq!(scale_of("remacri"), None);
        assert_eq!(scale_of("xenon"), None);
    }

    #[test]
    fn the_listing_is_the_top_level_weight_files_by_stem() {
        let dir = tempfile::tempdir().unwrap();
        let up = dir.path().join(DIR);
        std::fs::create_dir_all(up.join("nested")).unwrap();
        std::fs::write(up.join("RealESRGAN_x4plus.safetensors"), b"x").unwrap();
        std::fs::write(up.join("4x-other.pth"), b"xy").unwrap();
        std::fs::write(up.join("half.safetensors.part"), b"x").unwrap();
        std::fs::write(up.join("README.txt"), b"x").unwrap();
        std::fs::write(up.join("nested").join("deep.pth"), b"x").unwrap();
        let root = dir.path().display().to_string();
        assert_eq!(names(&root), vec!["4x-other", "RealESRGAN_x4plus"]);
        assert_eq!(on_disk(&root)[0].size_bytes, 2);
        assert!(names("").is_empty());
        let l = listing(&root, &[]);
        assert!(l.shipped[0].present, "{l:?}");
        assert_eq!(
            l.shipped[0].dest_path,
            "upscalers/RealESRGAN_x4plus.safetensors"
        );
    }

    #[test]
    fn the_shipped_entry_names_a_stem_the_enum_takes() {
        let u = find("realesrgan-x4plus").unwrap();
        assert_eq!(u.stem(), "RealESRGAN_x4plus");
        assert!(hf::accepts_file("image", u.file));
        assert!(hf::dest_rel_path(u.repo, u.file).is_ok());
    }
}
