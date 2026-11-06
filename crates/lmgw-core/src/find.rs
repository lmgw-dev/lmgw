//! Find a model by the name someone said (find-a-model design §4, WP4).
//!
//! [`find`] runs §4 against a [`Hub`]: the query read (§4.1, D124), the
//! hub's two searches (D12), the match (D13), the originals and their rank
//! (D14, D15, D126), and the walk to a file that fits the card by its size
//! (§4.3, D16, D17, D20), with the author's display name (D19). It reads no
//! header (D123) and needs no state, so the regression set runs it over
//! recorded answers (D38). [`card::answer`] turns what it found into
//! `model_find`'s typed answer with this install's card, rate and disk.
//!
//! Every signal is a field of the hub's live answer: downloads, likes, tags,
//! the `gguf` filter, `baseModels`, the author, the pipeline and sizes.
//! There is no list of names anywhere, and names are searched as typed
//! (D18).

pub mod card;
pub mod query;
pub mod rank;
pub mod walk;
pub mod words;

use std::collections::HashSet;

use lmgw_api_types::model_find::FindAnswer;

use crate::hf::search::{self, Budget, Fail, Hub, HubModel, Reads};

pub use query::{HubPage, Query};
pub use rank::{Kind, Original, Ranked};
pub use walk::{Asked, Chosen, Room, Stop};

/// What [`find`] found, before it is an answer.
#[derive(Debug, Clone)]
pub struct Found {
    pub query: String,
    pub parsed: Query,
    pub answer: FindAnswer,
    pub ranked: Ranked,
    /// The card's original (also `NothingFits`', `AllGated`'s and
    /// `NoRunnableFile`'s top one, for its name).
    pub origin: Option<Original>,
    /// The upload and its files, when the walk read them.
    pub chosen: Option<Chosen>,
    /// `AllGated`'s first gated upload.
    pub gated_upload: Option<String>,
    /// `NoFileInBand`'s upload, whose files are all outside the band (D71).
    pub outside_band: Option<Chosen>,
    /// `Unreachable` after the search was answered: the hub answered errors
    /// for every upload the walk asked about (the notes say which).
    pub walk_unanswered: bool,
    /// The hub's rate limit ran out partway through the walk (D76).
    pub stopped: Option<Stop>,
    /// How many originals the walk's set held (D17).
    pub walk_len: usize,
    /// Originals the walk found to have no GGUF anywhere.
    pub no_file: HashSet<String>,
    /// What the walk passed over, and anything that couldn't be read.
    pub notes: Vec<String>,
    /// The author's display name (D19).
    pub by: Option<String>,
    /// The author is a person's account, not an organisation's: its
    /// display name is self-chosen, so the card carries the id beside it.
    pub by_user: bool,
    pub next_cursor: Option<String>,
    /// The hub's requests this answer made, and its limit after them.
    pub calls: u32,
    pub budget: Budget,
    /// An address's repo the hub doesn't have (or doesn't show).
    pub missing_repo: Option<String>,
}

impl Found {
    fn new(query: &str, parsed: Query) -> Self {
        Self {
            query: query.to_string(),
            parsed,
            answer: FindAnswer::NoMatch,
            ranked: Ranked::default(),
            origin: None,
            chosen: None,
            gated_upload: None,
            outside_band: None,
            walk_unanswered: false,
            stopped: None,
            walk_len: 0,
            no_file: HashSet::new(),
            notes: Vec::new(),
            by: None,
            by_user: false,
            next_cursor: None,
            calls: 0,
            budget: Budget::default(),
            missing_repo: None,
        }
    }
}

/// §4 for `text` against `hub`, the walk measured against `room` (the
/// whole card; `None` without one, D9). `cursor` is a previous answer's
/// `next_cursor`. Refuses only what isn't a query: nothing to search for,
/// a cursor no answer gave.
pub async fn find<H: Hub>(
    hub: &H,
    text: &str,
    cursor: Option<&str>,
    room: Option<Room>,
) -> Result<Found, String> {
    let parsed = query::parse(text);
    let cursor = match cursor.map(str::trim).filter(|c| !c.is_empty()) {
        None => None,
        Some(c) => Some(search::decode_cursor(c).ok_or_else(|| {
            "cursor is not one a model_find answer gave as next_cursor".to_string()
        })?),
    };
    let mut found = Found::new(text.trim(), parsed.clone());
    let mut reads = Reads::new(hub);
    let run = match &parsed {
        Query::Empty => return Err("pass query: a model's name or a Hugging Face address".into()),
        Query::Elsewhere { .. } => {
            found.answer = FindAnswer::NotHuggingFace;
            Ok(())
        }
        Query::HubPage(_) => {
            found.answer = FindAnswer::NotAModel;
            Ok(())
        }
        Query::Name(q) => by_name(&mut reads, q, cursor, room, &mut found).await,
        Query::Repo(repo) => by_address(&mut reads, repo, None, None, room, &mut found).await,
        Query::Address { repo, file, rev } => {
            by_address(
                &mut reads,
                repo,
                file.clone(),
                rev.clone(),
                room,
                &mut found,
            )
            .await
        }
    };
    if let Err(fail) = run {
        match fail {
            Fail::RateLimited { .. } => found.answer = FindAnswer::RateLimited,
            Fail::Unreachable(why) => {
                found.answer = FindAnswer::Unreachable;
                found.notes.push(why);
            }
            Fail::Status { status, body } => {
                found.answer = FindAnswer::Unreachable;
                let first = body.lines().map(str::trim).find(|l| !l.is_empty());
                found.notes.push(format!(
                    "Hugging Face answered {status}{}",
                    first.map(|l| format!(": {l}")).unwrap_or_default()
                ));
            }
        }
    }
    found.calls = reads.calls;
    found.budget = reads.budget;
    Ok(found)
}

/// A name (§4.2, then §4.3).
async fn by_name<H: Hub>(
    reads: &mut Reads<'_, H>,
    q: &str,
    cursor: Option<(Option<String>, Option<String>)>,
    room: Option<Room>,
    found: &mut Found,
) -> Result<(), Fail> {
    let more = cursor.is_some();
    let page = search::search(reads, q, cursor).await?;
    found.next_cursor = search::encode_cursor(page.next_all.as_deref(), page.next_gguf.as_deref());
    let words = words::words(q);
    found.ranked = rank::rank(rank::union(page.all, page.gguf), &words);
    if more {
        found.answer = FindAnswer::More;
        return Ok(());
    }
    let Some(top) = found.ranked.originals.first().cloned() else {
        found.answer = match found.ranked.partial.is_empty() {
            true => FindAnswer::NoMatch,
            false => FindAnswer::LooseOnly,
        };
        return Ok(());
    };
    // A kind Find a model sets up ranks first (D15), so the top is another
    // kind only when no original of a kind it sets up matched.
    if let Kind::Other(_) = top.kind {
        found.origin = Some(top);
        found.answer = FindAnswer::OtherKind;
        return Ok(());
    }
    let set = rank::walk_set(&found.ranked.originals);
    let set_owned: Vec<Original> = set.into_iter().cloned().collect();
    let refs: Vec<&Original> = set_owned.iter().collect();
    let walked = walk::walk(reads, &refs, room, None).await?;
    settle(reads, walked, &set_owned, top, found).await;
    Ok(())
}

/// Why a read failed, in a few words for a note.
fn fail_words(e: &Fail) -> String {
    match e {
        Fail::RateLimited { .. } => "Hugging Face's rate limit".to_string(),
        Fail::Unreachable(w) => w.clone(),
        Fail::Status { status, .. } => format!("Hugging Face answered {status}"),
    }
}

/// The walk's end, into `found`, and the author's display name for a card.
async fn settle<H: Hub>(
    reads: &mut Reads<'_, H>,
    walked: walk::Walked,
    set: &[Original],
    top: Original,
    found: &mut Found,
) {
    found.notes.extend(walked.steps);
    found.no_file = walked.no_file;
    found.walk_len = set.len();
    found.stopped = walked.stopped;
    if let Some(stop) = walked.stopped {
        found.notes.push(stop_note(&stop, &top));
    }
    let card = match walked.outcome {
        walk::Outcome::Card { origin, chosen } => {
            found.answer = FindAnswer::Card;
            found.origin = Some(set[origin].clone());
            found.chosen = Some(chosen);
            true
        }
        walk::Outcome::NothingFits { origin, chosen } => {
            found.answer = FindAnswer::NothingFits;
            found.origin = Some(set[origin].clone());
            found.chosen = chosen;
            true
        }
        walk::Outcome::AllGated { origin, upload } => {
            found.answer = FindAnswer::AllGated;
            found.origin = Some(set[origin].clone());
            found.gated_upload = Some(upload);
            false
        }
        walk::Outcome::OutsideBand { origin, chosen } => {
            found.answer = FindAnswer::NoFileInBand;
            found.origin = Some(set[origin].clone());
            found.outside_band = Some(chosen);
            false
        }
        walk::Outcome::Unreadable => {
            found.answer = FindAnswer::Unreachable;
            found.walk_unanswered = true;
            found.origin = Some(top);
            false
        }
        walk::Outcome::Stopped => {
            found.answer = FindAnswer::RateLimited;
            found.origin = Some(top);
            false
        }
        walk::Outcome::NoRunnableFile => {
            found.answer = FindAnswer::NoRunnableFile;
            found.origin = Some(top);
            false
        }
    };
    if !card {
        return;
    }
    let author = found
        .origin
        .as_ref()
        .map(|o| o.model.author().to_string())
        .unwrap_or_default();
    // "by …" is the card's last read: when it fails, the card says the id.
    match search::display_name(reads, &author).await {
        Ok(account) => {
            found.by = account.name;
            found.by_user = account.user;
        }
        Err(e) => found.notes.push(format!(
            "the display name of {author} couldn't be read ({}), so the card names its id",
            fail_words(&e)
        )),
    }
}

/// The note when the hub's limit stopped the walk (D76): what was checked,
/// and when the limit resets.
fn stop_note(stop: &Stop, top: &Original) -> String {
    let resets = stop
        .reset_s
        .map(|s| format!(" It resets in {}.", card::duration_words(s)))
        .unwrap_or_default();
    if stop.of < 2 {
        return format!(
            "Hugging Face's limit on requests ran out before lmgw had read the files of {}.\
             {resets}",
            top.id()
        );
    }
    format!(
        "Hugging Face's limit on requests ran out partway: lmgw checked {} of the {} models \
         by {} with this name, and the others weren't checked.{resets}",
        stop.checked,
        stop.of,
        top.model.author()
    )
}

/// An address or `owner/repo` (§4.1): straight to §4.3 with that repo, and
/// with the file and revision an address names. An address is never
/// passed over by its count (D77).
async fn by_address<H: Hub>(
    reads: &mut Reads<'_, H>,
    repo: &str,
    file: Option<String>,
    rev: Option<String>,
    room: Option<Room>,
    found: &mut Found,
) -> Result<(), Fail> {
    let Some(m) = search::model(reads, repo).await? else {
        found.missing_repo = Some(repo.to_string());
        found.answer = FindAnswer::NoMatch;
        return Ok(());
    };
    let gguf_only = !m.own_weights() && m.has_tag("gguf");
    // A GGUF-only repo is an upload; its original is the model it quantizes,
    // read for its name, count and licence.
    let (origin_model, asked) = if gguf_only {
        let (relation, bases) = m.bases();
        let base = match (relation, bases.as_slice()) {
            // The original is the card's extra: when it can't be read, the
            // upload is its own original (D79).
            (Some("quantized"), [one]) => match search::model(reads, one).await {
                Ok(b) => b,
                Err(e @ Fail::RateLimited { .. }) => return Err(e),
                Err(e) => {
                    found.notes.push(format!(
                        "{one}, the model {repo} is made from, couldn't be read ({}), so the \
                         card names {repo}",
                        fail_words(&e)
                    ));
                    None
                }
            },
            _ => None,
        };
        let asked = Asked {
            upload: Some(m.clone()),
            file,
            rev,
        };
        (base.unwrap_or_else(|| m.clone()), asked)
    } else {
        // An original: its uploads, unless the address names one of its
        // own files.
        let asked = Asked {
            upload: file.is_some().then(|| m.clone()),
            file,
            rev,
        };
        (m.clone(), asked)
    };
    let origin = address_original(origin_model);
    if let Kind::Other(_) = origin.kind {
        found.origin = Some(origin);
        found.answer = FindAnswer::OtherKind;
        return Ok(());
    }
    found.ranked.originals = vec![origin.clone()];
    let set = [origin.clone()];
    let walked = walk::walk(reads, &[&set[0]], room, Some(asked)).await?;
    settle(reads, walked, &set, origin, found).await;
    Ok(())
}

fn address_original(model: HubModel) -> Original {
    Original {
        kind: Kind::of(&model),
        matched: words::Match::default(),
        makers_gguf: None,
        model,
    }
}
