//! Finding the file (find-a-model design §4.3, D16, D17, D20, D126): the
//! walk through the top author's originals to the first with an upload whose
//! file fits the card by its size.
//!
//! No header is read here (D123): a file's width is its bytes over the
//! listing's parameter total, and its mark is by size. The page then asks
//! `hf_repo` with `fit=true` for the exact one.

use std::collections::HashSet;

use crate::hf::search::{self, Fail, Hub, HubModel, Reads};
use crate::hf::{self, HfFile};
use crate::modelinfo::pick::{CAP_BITS, FLOOR_BITS};

use super::rank::{strip_gguf, Original};

/// The width below which a whole file still reads as the Q8 class without a
/// header (the WP4 call D71): the midpoint between Q8_0's 8.5 bits and the
/// 16 of F16 and BF16, the next width a GGUF's weights come in. A file's
/// bytes over the listing's count carry its tokenizer, norms and embeddings
/// too, so a real Q8_0 reads a little over 8.5 (a 0.6B model's by about
/// 0.07), and nothing between the two classes is a file type.
pub const WHOLE_FILE_CAP_BITS: f64 = (CAP_BITS + 16.0) / 2.0;

/// The card the walk measures against: the whole card (D5), as admission
/// measures it, and the headroom added to every file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    pub capacity: u64,
    pub headroom: u64,
}

impl Room {
    /// Whether `bytes` and the headroom fit.
    pub fn holds(&self, bytes: u64) -> bool {
        bytes.saturating_add(self.headroom) <= self.capacity
    }
}

/// One weights file of an upload: a split file is one, its parts summed.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightsFile {
    /// Its path in the repo (a split file's first part).
    pub file: String,
    pub parts: u32,
    /// Every part's bytes; `None` when the tree lacks a part or a size.
    pub bytes: Option<u64>,
    /// Bits per weight over the whole file, by the listing's parameter
    /// total; `None` without one.
    pub bits: Option<f64>,
}

impl WeightsFile {
    /// Whether the size pick may take it: 4 bits per weight to the Q8 class
    /// by its whole-file width, or any width without a count (the header's
    /// count caps it then, D7).
    pub fn in_band(&self) -> bool {
        self.bits
            .is_none_or(|b| (FLOOR_BITS..WHOLE_FILE_CAP_BITS).contains(&b))
    }
}

/// The weights files of a tree, split parts grouped (as `hf_repo` lists
/// them), with their widths by `parameters`.
pub fn weights_files(tree: &[HfFile], parameters: Option<u64>) -> Vec<WeightsFile> {
    let mut out: Vec<WeightsFile> = Vec::new();
    let mut seen = HashSet::new();
    for f in tree {
        if crate::ops::classify_repo_file(&f.path, "chat") != "weights" {
            continue;
        }
        let parts = hf::expand_parts(&f.path, tree);
        let first = parts.first().cloned().unwrap_or_else(|| f.path.clone());
        if !seen.insert(first.clone()) {
            continue;
        }
        let declared = hf::split_part_name(&first).map(|(_, _, n)| n).unwrap_or(1);
        let sizes: Vec<Option<u64>> = parts
            .iter()
            .map(|p| tree.iter().find(|x| &x.path == p).and_then(|x| x.size))
            .collect();
        let bytes = (sizes.len() as u32 == declared && sizes.iter().all(Option::is_some))
            .then(|| sizes.iter().flatten().sum::<u64>());
        let bits = match (bytes, parameters) {
            (Some(b), Some(p)) if p > 0 => Some(b as f64 * 8.0 / p as f64),
            _ => None,
        };
        out.push(WeightsFile {
            file: first,
            parts: parts.len().max(1) as u32,
            bytes,
            bits,
        });
    }
    out.sort_by(|a, b| a.file.cmp(&b.file));
    out
}

/// The size pick (D123's provisional one): the largest file in the band
/// that `room_bytes` holds with the headroom, equal sizes by name; without
/// a card, the largest in the band.
pub fn pick_by_size(files: &[WeightsFile], room: Option<Room>) -> Option<&WeightsFile> {
    files
        .iter()
        .filter(|f| f.in_band())
        .filter_map(|f| f.bytes.map(|b| (f, b)))
        .filter(|(_, b)| room.is_none_or(|r| r.holds(*b)))
        .max_by(|(a, ab), (b, bb)| ab.cmp(bb).then_with(|| b.file.cmp(&a.file)))
        .map(|(f, _)| f)
}

/// The smallest file in the band, with its bytes.
pub fn smallest(files: &[WeightsFile]) -> Option<(&WeightsFile, u64)> {
    files
        .iter()
        .filter(|f| f.in_band())
        .filter_map(|f| f.bytes.map(|b| (f, b)))
        .min_by(|(a, ab), (b, bb)| ab.cmp(bb).then_with(|| a.file.cmp(&b.file)))
}

/// Whether `upload` is a plain upload of `origin` (D16): the original's
/// name, optionally with a prefix and a `-GGUF` ending.
pub fn plain_upload(upload: &str, origin: &str) -> bool {
    let name = |id: &str| {
        id.split_once('/')
            .map(|(_, n)| n)
            .unwrap_or(id)
            .to_ascii_lowercase()
    };
    let up = name(upload);
    let up = strip_gguf(&up);
    let o = name(origin);
    up == o
        || up
            .strip_suffix(o.as_str())
            .is_some_and(|prefix| prefix.ends_with(['-', '_']))
}

/// The band in words for a newcomer (D71): what the size pick takes, and
/// why a whole file's width by its size reads past 8.5.
pub fn band_words() -> String {
    format!(
        "lmgw picks a file of 4 to 8 bits per weight: narrower files lose too much of the \
         model, and wider ones are the full 16- or 32-bit weights, nearly twice the size for \
         little gain. Until it has read a file's header lmgw goes by the file's size, and a \
         file under {WHOLE_FILE_CAP_BITS} bits per weight by its size still counts as 8-bit, \
         since a whole file carries more than the weights."
    )
}

/// Why none of `files` is in the band, as the rest of "every file is …".
pub fn outside_band(files: &[WeightsFile]) -> String {
    let narrow = files.iter().any(|f| f.bits.is_some_and(|b| b < FLOOR_BITS));
    let wide = files
        .iter()
        .any(|f| f.bits.is_some_and(|b| b >= WHOLE_FILE_CAP_BITS));
    let no_size = files.iter().any(|f| f.in_band() && f.bytes.is_none());
    let mut parts = Vec::new();
    if narrow {
        parts.push("narrower than 4 bits per weight, which loses too much of the model");
    }
    if wide {
        parts
            .push("the full 16- or 32-bit weights, nearly twice the size of an 8-bit file or more");
    }
    if no_size {
        parts.push("split into parts Hugging Face doesn't list in full, so its size isn't known");
    }
    parts.join(", or ")
}

/// What an address asks the walk for (§4.1, D77): the upload it names (a
/// GGUF repo, or the original's own repo when the address names a file in
/// it), the file and the revision it names. An address is never passed
/// over by its count.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    pub upload: Option<HubModel>,
    pub file: Option<String>,
    pub rev: Option<String>,
}

/// The upload chosen for a card, with what the walk read of it.
#[derive(Debug, Clone)]
pub struct Chosen {
    pub upload: HubModel,
    /// The tree at `main`.
    pub tree: Vec<HfFile>,
    pub files: Vec<WeightsFile>,
    /// The original's plain uploads after this one, in D16's order.
    pub other_plain: Vec<HubModel>,
    /// Its renamed uploads (an MTP variant, an abliterated one).
    pub renamed: Vec<HubModel>,
    /// The hub has another page of uploads.
    pub more_uploads: bool,
    /// A file the address named, when it is one of `files` (§4.1).
    pub asked_file: Option<String>,
}

impl Chosen {
    /// The address's file, when it is one of `files`.
    pub fn asked(&self) -> Option<&WeightsFile> {
        let a = self.asked_file.as_ref()?;
        self.files.iter().find(|f| &f.file == a)
    }
}

/// Whether `file` fits `room` by its size; any file without a card (D9).
pub fn fits(file: &WeightsFile, room: Option<Room>) -> bool {
    match (room, file.bytes) {
        (None, _) => true,
        (Some(r), Some(b)) => r.holds(b),
        (Some(_), None) => false,
    }
}

/// Where the walk ended for the card.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// An original with a file that fits (`origin` indexes the walk's set).
    Card { origin: usize, chosen: Chosen },
    /// No original of the walk has a file that fits: the one closest to
    /// fitting (its smallest file, or its size at 4 bits per weight when it
    /// was passed over), with its upload when the walk read one.
    NothingFits {
        origin: usize,
        chosen: Option<Chosen>,
    },
    /// No original has a file in the band (D71), and the first upload that
    /// had weights at all is `chosen`.
    OutsideBand { origin: usize, chosen: Chosen },
    /// Every upload the walk could use is gated and its tree wasn't
    /// readable: the first one's repo.
    AllGated { origin: usize, upload: String },
    /// Hugging Face answered the search but not the uploads' files: every
    /// one the walk asked about answered an error (`steps` say which).
    Unreadable,
    /// The hub's rate limit ran out before the walk found a file or a size
    /// (`Walked::stopped`).
    Stopped,
    /// No original of the walk has a GGUF anywhere.
    NoRunnableFile,
}

/// Where the hub's rate limit stopped the walk (D76).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop {
    pub reset_s: Option<u64>,
    /// The originals walked before the limit, of the set's.
    pub checked: usize,
    pub of: usize,
}

/// The walk's end and what it passed on the way.
#[derive(Debug, Clone)]
pub struct Walked {
    pub outcome: Outcome,
    /// One sentence per original or upload passed over, and why.
    pub steps: Vec<String>,
    /// The originals found to have no GGUF anywhere.
    pub no_file: HashSet<String>,
    /// The hub's rate limit ran out partway: what was found is kept.
    pub stopped: Option<Stop>,
}

/// An original's uploads in D16's and D20's order: the open ones first, the
/// maker's own first among them, then by likes and downloads; and its
/// renamed uploads.
async fn uploads_of<H: Hub>(
    reads: &mut Reads<'_, H>,
    o: &Original,
) -> Result<(Vec<HubModel>, Vec<HubModel>, bool), Fail> {
    if !o.model.own_weights() {
        // A GGUF-only original is its own upload.
        return Ok(match o.model.has_tag("gguf") {
            true => (vec![o.model.clone()], Vec::new(), false),
            false => (Vec::new(), Vec::new(), false),
        });
    }
    let (listed, more) = search::uploads(reads, o.id()).await?;
    let (mut plain, renamed): (Vec<HubModel>, Vec<HubModel>) = listed
        .into_iter()
        .filter(|u| {
            o.makers_gguf
                .as_ref()
                .is_none_or(|m| !m.id.eq_ignore_ascii_case(&u.id))
        })
        .partition(|u| plain_upload(&u.id, o.id()));
    if let Some(m) = &o.makers_gguf {
        plain.push(m.clone());
    }
    let maker = o.model.author().to_ascii_lowercase();
    plain.sort_by_key(|u| {
        (
            u.gated(),
            !u.author().eq_ignore_ascii_case(&maker),
            std::cmp::Reverse((u.likes, u.downloads)),
            u.id.clone(),
        )
    });
    Ok((plain, renamed, more))
}

/// How one original fared.
enum Tried {
    Fits(Chosen),
    /// Its smallest file in the band doesn't fit: that file's bytes.
    TooBig {
        chosen: Chosen,
        needs: u64,
    },
    /// Uploads with weights, none in the band: the first.
    Outside(Chosen),
    Gated(String),
    /// Every upload asked about answered an error.
    Unreadable,
    NoFile,
}

/// The note for an address's file that isn't one of the upload's weights
/// (D77): not in `main`'s tree, or not a weights file.
fn named_note(named: &str, rev: Option<&str>, tree: &[HfFile], upload: &str) -> String {
    if tree.iter().any(|f| f.path == named) {
        return format!(
            "{named}, the file the address names, isn't the model's weights, so the card shows \
             a weights file of {upload}."
        );
    }
    let revision = rev
        .filter(|r| *r != "main")
        .map(|r| format!(" The address names the revision {r}; lmgw reads the main branch."))
        .unwrap_or_default();
    format!(
        "{named}, the file the address names, isn't among the files of {upload} on its main \
         branch, so the card shows another file of it.{revision}"
    )
}

/// The uploads `ups` in turn: the first one's tree that has weights in the
/// band, and the size pick. `named` is an address's file.
#[allow(clippy::too_many_arguments)]
async fn scan<H: Hub>(
    reads: &mut Reads<'_, H>,
    o: &Original,
    room: Option<Room>,
    ups: &[HubModel],
    renamed: &[HubModel],
    more: bool,
    named: Option<&str>,
    rev: Option<&str>,
    steps: &mut Vec<String>,
) -> Result<Tried, Fail> {
    let mut first_gated = None;
    let mut outside: Option<Chosen> = None;
    let mut unreadable = false;
    for (i, up) in ups.iter().enumerate() {
        let tree = match search::tree(reads, &up.id).await {
            Ok(t) => t,
            // A gated repo answers 401 (403 with a token that hasn't
            // accepted) to its tree too.
            Err(Fail::Status {
                status: 401 | 403, ..
            }) => {
                steps.push(format!(
                    "{}: {} asks people to accept its terms on Hugging Face first, so lmgw \
                     couldn't read its files",
                    o.id(),
                    up.id
                ));
                first_gated.get_or_insert_with(|| up.id.clone());
                continue;
            }
            // One upload the hub can't answer for (gone, blocked, a server
            // error) is passed for the next; the search itself was answered.
            Err(Fail::Status { status, .. }) => {
                steps.push(format!(
                    "{}: Hugging Face answered {status} when lmgw asked for the files of {}, \
                     so lmgw passed it",
                    o.id(),
                    up.id
                ));
                unreadable = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        let files = weights_files(&tree, o.model.parameters());
        if files.is_empty() {
            continue;
        }
        let asked_file = named.and_then(|a| {
            files
                .iter()
                .find(|f| f.file == a || hf::same_split_set(&f.file, a))
                .map(|f| f.file.clone())
        });
        if let (Some(n), None) = (named, &asked_file) {
            steps.push(named_note(n, rev, &tree, &up.id));
        }
        let chosen = Chosen {
            upload: up.clone(),
            tree,
            other_plain: ups[i + 1..].to_vec(),
            renamed: renamed.to_vec(),
            more_uploads: more,
            asked_file,
            files,
        };
        // The address's own file is the card's when it fits, whatever its
        // width (D77).
        if let Some(a) = chosen.asked() {
            if fits(a, room) {
                return Ok(Tried::Fits(chosen));
            }
            steps.push(match (a.bytes, room) {
                (Some(b), Some(r)) => format!(
                    "{}, the file the address names, needs {} with the headroom, and the card \
                     has {}",
                    a.file,
                    hf::fmt_bytes(b.saturating_add(r.headroom)),
                    hf::fmt_bytes(r.capacity)
                ),
                _ => format!(
                    "{}, the file the address names, is split into parts Hugging Face doesn't \
                     list in full, so lmgw can't tell whether it fits",
                    a.file
                ),
            });
        }
        let Some((small, bytes)) = smallest(&chosen.files) else {
            // Like an upload without weights: the next one may have a file
            // in the band.
            steps.push(format!(
                "{}: lmgw picks no file of {} by itself: every file is {}",
                o.id(),
                up.id,
                outside_band(&chosen.files)
            ));
            outside.get_or_insert(chosen);
            continue;
        };
        if room.is_some_and(|r| !r.holds(bytes)) {
            steps.push(format!(
                "{}: its smallest file, {} from {}, needs {} with the headroom",
                o.id(),
                small.file,
                up.id,
                hf::fmt_bytes(bytes.saturating_add(room.map_or(0, |r| r.headroom)))
            ));
            return Ok(Tried::TooBig {
                chosen,
                needs: bytes,
            });
        }
        return Ok(Tried::Fits(chosen));
    }
    Ok(match (outside, first_gated) {
        (Some(c), _) => Tried::Outside(c),
        (None, Some(g)) => Tried::Gated(g),
        (None, None) if unreadable => Tried::Unreadable,
        (None, None) => Tried::NoFile,
    })
}

/// The walk for one original: its uploads (or the address's), and the
/// first one with weights in the band.
async fn try_original<H: Hub>(
    reads: &mut Reads<'_, H>,
    o: &Original,
    room: Option<Room>,
    asked: Option<&Asked>,
    steps: &mut Vec<String>,
) -> Result<Tried, Fail> {
    let named = asked.and_then(|a| a.file.as_deref());
    let rev = asked.and_then(|a| a.rev.as_deref());
    if let Some(up) = asked.and_then(|a| a.upload.as_ref()) {
        let tried = scan(
            reads,
            o,
            room,
            std::slice::from_ref(up),
            &[],
            false,
            named,
            rev,
            steps,
        )
        .await?;
        // An address that names a file of an original's own repo, which
        // holds no GGUF (its config, a safetensors file): the original's
        // uploads, as for its name.
        let own = up.id.eq_ignore_ascii_case(o.id()) && o.model.own_weights();
        if !(own && matches!(tried, Tried::NoFile)) {
            return Ok(tried);
        }
        steps.push(format!(
            "{}, the file the address names, isn't one lmgw can run: lmgw runs GGUF files, and \
             {} has none, so lmgw looked at the GGUF uploads of it",
            named.unwrap_or_default(),
            up.id
        ));
    }
    let (ups, renamed, more) = match uploads_of(reads, o).await {
        Ok(u) => u,
        Err(Fail::Status { status, .. }) => {
            steps.push(format!(
                "{}: Hugging Face answered {status} when lmgw asked for its uploads, so lmgw \
                 passed it",
                o.id()
            ));
            return Ok(Tried::Unreadable);
        }
        Err(e) => return Err(e),
    };
    if ups.is_empty() {
        return Ok(Tried::NoFile);
    }
    scan(reads, o, room, &ups, &renamed, more, None, None, steps).await
}

/// The walk of §4.3 over `set` (the top author's originals of the top's
/// kind and match, in their order, D17): the first with a file that fits.
/// For a name, an original is passed over with no call when its listed
/// parameter count at 4 bits per weight with the headroom is more than the
/// card; an address (`asked`) is never passed over.
///
/// What it costs: two calls per original it reads (its uploads and the
/// first upload's tree), and one more for every gated, weightless or
/// unanswered upload before the one it uses, and for every further page of
/// a long tree. lmgw sets no bound of its own: the hub's limit is the bound
/// (`Reads`), and when it runs out partway the walk keeps what it found
/// (`Walked::stopped`).
pub async fn walk<H: Hub>(
    reads: &mut Reads<'_, H>,
    set: &[&Original],
    room: Option<Room>,
    asked: Option<Asked>,
) -> Result<Walked, Fail> {
    let mut steps = Vec::new();
    let mut no_file = HashSet::new();
    let mut too_big: Vec<(usize, Option<Chosen>, u64)> = Vec::new();
    let mut outside: Option<(usize, Chosen)> = None;
    let mut gated: Option<(usize, String)> = None;
    let mut unreadable = false;
    let mut stopped = None;
    for (i, o) in set.iter().enumerate() {
        if let (None, Some(r), Some(total)) = (&asked, room, o.model.parameters()) {
            let at_floor = (total as f64 * FLOOR_BITS / 8.0) as u64;
            if !r.holds(at_floor) {
                steps.push(format!(
                    "{}: {} parameters need at least {} at {FLOOR_BITS} bits per weight, with \
                     the headroom; passed over without asking for its files",
                    o.id(),
                    super::card::parameters_words(total),
                    hf::fmt_bytes(at_floor.saturating_add(r.headroom))
                ));
                too_big.push((i, None, at_floor));
                continue;
            }
        }
        let tried = match try_original(reads, o, room, asked.as_ref(), &mut steps).await {
            Ok(t) => t,
            Err(Fail::RateLimited { reset_s }) => {
                stopped = Some(Stop {
                    reset_s,
                    checked: i,
                    of: set.len(),
                });
                break;
            }
            Err(e) => return Err(e),
        };
        match tried {
            Tried::Fits(chosen) => {
                return Ok(Walked {
                    outcome: Outcome::Card { origin: i, chosen },
                    steps,
                    no_file,
                    stopped: None,
                })
            }
            Tried::TooBig { chosen, needs } => too_big.push((i, Some(chosen), needs)),
            Tried::Outside(chosen) => {
                outside.get_or_insert((i, chosen));
            }
            Tried::Gated(up) => {
                gated.get_or_insert((i, up));
            }
            Tried::Unreadable => unreadable = true,
            Tried::NoFile => {
                steps.push(format!("{}: no file lmgw can run yet", o.id()));
                no_file.insert(o.id().to_string());
            }
        }
    }
    // The one closest to fitting, the first of equals (D17, refined).
    let closest = too_big.into_iter().min_by_key(|(_, _, needs)| *needs);
    let outcome = match (closest, stopped, outside, gated) {
        (Some((origin, chosen, _)), ..) => Outcome::NothingFits { origin, chosen },
        (None, Some(_), ..) => Outcome::Stopped,
        (None, None, Some((origin, chosen)), _) => Outcome::OutsideBand { origin, chosen },
        (None, None, None, Some((origin, upload))) => Outcome::AllGated { origin, upload },
        (None, None, None, None) if unreadable => Outcome::Unreadable,
        (None, None, None, None) => Outcome::NoRunnableFile,
    };
    Ok(Walked {
        outcome,
        steps,
        no_file,
        stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> HfFile {
        HfFile {
            path: path.into(),
            size: Some(size),
            kind: "file".into(),
            lfs: None,
        }
    }

    #[test]
    fn a_plain_upload_is_the_originals_name_with_a_prefix_and_an_ending() {
        let o = "maker-demo/Demo-2-8B";
        assert!(plain_upload("fan-demo/Demo-2-8B-GGUF", o));
        assert!(plain_upload("fan-demo/maker-demo_Demo-2-8B-GGUF", o));
        assert!(plain_upload("maker-demo/Demo-2-8B-gguf", o));
        assert!(!plain_upload("fan-demo/Demo-2-8B-abliterated-GGUF", o));
        assert!(!plain_upload("fan-demo/XDemo-2-8B-GGUF", o));
    }

    #[test]
    fn the_size_pick_keeps_the_band_and_the_room() {
        let gib = 1u64 << 30;
        let tree = vec![
            file("m-Q4_K_M.gguf", 5 * gib),
            file("m-Q8_0.gguf", 9 * gib),
            file("m-F16.gguf", 17 * gib),
            file("m-Q2_K.gguf", 3 * gib),
            file("mmproj-m-F16.gguf", gib),
            file("big-00001-of-00002.gguf", 6 * gib),
            file("big-00002-of-00002.gguf", 6 * gib),
        ];
        // 8 B parameters: Q2_K is 3.2 bits, the F16 17.2.
        let files = weights_files(&tree, Some(8_000_000_000));
        let names: Vec<&str> = files.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(
            names,
            [
                "big-00001-of-00002.gguf",
                "m-F16.gguf",
                "m-Q2_K.gguf",
                "m-Q4_K_M.gguf",
                "m-Q8_0.gguf"
            ]
        );
        assert_eq!(files[0].bytes, Some(12 * gib));
        assert_eq!(files[0].parts, 2);
        let room = |c| {
            Some(Room {
                capacity: c * gib,
                headroom: gib,
            })
        };
        // The 12 GiB split file is 12.9 bits: past the Q8 class.
        assert_eq!(pick_by_size(&files, room(24)).unwrap().file, "m-Q8_0.gguf");
        assert_eq!(pick_by_size(&files, room(8)).unwrap().file, "m-Q4_K_M.gguf");
        assert!(pick_by_size(&files, room(4)).is_none());
        assert_eq!(pick_by_size(&files, None).unwrap().file, "m-Q8_0.gguf");
        assert_eq!(smallest(&files).unwrap().0.file, "m-Q4_K_M.gguf");
        // A split file missing a part has no size and is never picked.
        let cut = weights_files(&tree[..6], None);
        assert_eq!(cut[0].bytes, None);
    }
}
