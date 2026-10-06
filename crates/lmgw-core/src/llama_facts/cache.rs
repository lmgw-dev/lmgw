//! The external rows' `/props` cache (llama egress design §4.2).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;

use super::probe::{ask_model, ask_server, Answer};
use crate::config::Upstream;
use crate::egress::llama_cpp::props::LlamaFacts;

/// What one probe is about: the row, the base URL it was asked at and, under
/// a router only, one model. The base URL is part of it, so a row pointed
/// elsewhere never reads the old server's facts.
///
/// A llama-server that is no router serves its one model whatever name a
/// request carries, so what it says is the server's: one key, `model: None`,
/// however many names an `expose_all` row passes through. Only a router
/// states facts per model, and only under its answer is a model a key of its
/// own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FactsKey {
    pub row: i64,
    pub base_url: String,
    /// `None` for the server itself; one model of a router otherwise.
    pub model: Option<String>,
}

impl FactsKey {
    /// The server the row `up` points at.
    pub fn server(up: &Upstream) -> Self {
        Self {
            row: up.id,
            base_url: up.base().to_string(),
            model: None,
        }
    }

    /// One model of the router this key's server is.
    pub fn model(&self, model: &str) -> Self {
        Self {
            model: Some(model.to_string()),
            ..self.clone()
        }
    }
}

/// One probe's answer and when it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsEntry {
    pub answer: Answer,
    pub at: DateTime<Utc>,
}

/// One key of a row, as the surfaces show it ([`ExternalFacts::row`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyState {
    pub key: FactsKey,
    /// The last answer, if one came since the row was last invalidated.
    pub entry: Option<FactsEntry>,
    /// Waiting for the row's probe, or being asked now.
    pub probing: bool,
}

/// The external `llama_cpp` rows' `/props` facts (§4.2): asked in the
/// background, one probe per row at a time, kept until an event says they may
/// be stale. No TTL — facts change when the server process does, and each
/// way lmgw can tell that is an invalidation ([`Self::invalidate`]).
///
/// Shared with the probe tasks, which hold the inside rather than the
/// gateway state: a task never keeps a gateway alive.
#[derive(Debug, Default)]
pub struct ExternalFacts {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<FactsKey, FactsEntry>,
    rows: HashMap<i64, RowProbe>,
}

impl Inner {
    /// The answer kept for `key`; a [`Answer::Retry`] is none.
    fn kept(&self, key: &FactsKey) -> Option<&Answer> {
        self.entries
            .get(key)
            .map(|e| &e.answer)
            .filter(|a| a.cached())
    }
}

/// One row's probing.
#[derive(Debug, Default)]
struct RowProbe {
    /// Bumped by every invalidation, which also aborts the row's task: a
    /// task runs under one generation and, once the row's has moved on,
    /// stops without touching anything — an answer it already holds is
    /// dropped, never stored.
    generation: u64,
    /// The keys waiting, each with the row as it was when it was asked for.
    queue: VecDeque<(FactsKey, Upstream)>,
    /// The key asked now.
    asking: Option<FactsKey>,
    /// The models asked about while the server's own answer is outstanding:
    /// each asked in turn if it answers as a router, forgotten otherwise.
    then: BTreeSet<String>,
    /// A task is draining this row's queue.
    running: bool,
    /// That task, for [`ExternalFacts::settled`] and for an invalidation to
    /// abort.
    task: Option<JoinHandle<()>>,
}

impl RowProbe {
    /// Whether `key` is waiting or asked now.
    fn covers(&self, key: &FactsKey) -> bool {
        self.asking.as_ref() == Some(key) || self.queue.iter().any(|(k, _)| k == key)
    }
}

impl ExternalFacts {
    /// What the row `up` serves `model` with, as far as it is known — never
    /// waiting. Unknown (`None`) the first time, when the server answered
    /// without facts, and while a probe that found nothing waits for the next
    /// use; each of those but a kept answer queues a background probe: of the
    /// server, or of `model` when the server is a router.
    pub fn lookup(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        model: &str,
    ) -> Option<Arc<LlamaFacts>> {
        let server = FactsKey::server(up);
        let mut inner = self.lock();
        let key = match inner.kept(&server).cloned() {
            Some(Answer::Router(_)) => {
                let key = server.model(model);
                if let Some(answer) = inner.kept(&key) {
                    return answer.facts().cloned();
                }
                key
            }
            Some(answer) => return answer.facts().cloned(),
            None => {
                let row = inner.rows.entry(up.id).or_default();
                row.then.insert(model.to_string());
                server
            }
        };
        self.queue(&mut inner, http, key, up);
        None
    }

    /// Put `key` in its row's queue, unless it is there or asked now, and
    /// start the row's task unless it runs.
    fn queue(&self, inner: &mut Inner, http: &reqwest::Client, key: FactsKey, up: &Upstream) {
        let row = inner.rows.entry(key.row).or_default();
        // A task that ended without saying so panicked: start over.
        if row.running && row.task.as_ref().is_some_and(JoinHandle::is_finished) {
            row.running = false;
            row.asking = None;
        }
        if !row.covers(&key) {
            row.queue.push_back((key, up.clone()));
        }
        if !row.running {
            row.running = true;
            let task = drain(self.inner.clone(), http.clone(), up.id, row.generation);
            row.task = Some(tokio::spawn(task));
        }
    }

    /// Forget everything row `row` said, and abort a probe still on its way
    /// (§4.2's four events): a server that never answers, under a row with
    /// no deadline (`timeout_ms = 0`), holds the row's probing only until
    /// the next of them. The models it knew or was about to ask a router
    /// about, for a re-probe.
    pub fn invalidate(&self, row: i64) -> Vec<String> {
        let mut inner = self.lock();
        let mut models: Vec<String> = Vec::new();
        inner.entries.retain(|k, _| {
            if k.row != row {
                return true;
            }
            models.extend(k.model.clone());
            false
        });
        if let Some(probe) = inner.rows.get_mut(&row) {
            probe.generation += 1;
            models.extend(probe.queue.drain(..).filter_map(|(k, _)| k.model));
            models.extend(probe.asking.take().and_then(|k| k.model));
            models.extend(std::mem::take(&mut probe.then));
            probe.running = false;
            if let Some(task) = probe.task.take() {
                task.abort();
            }
        }
        models.sort();
        models.dedup();
        models
    }

    /// The row's Test button (§4.2): forget what the row said and ask its
    /// server again, now — a row never asked before included — and, if it
    /// answers as a router, about `models` (the models the row's aliases
    /// name) and every model it had been asked about. Nothing for a row of
    /// another protocol.
    pub fn retest(
        &self,
        http: &reqwest::Client,
        up: &Upstream,
        models: impl IntoIterator<Item = String>,
    ) {
        let known = self.invalidate(up.id);
        if !super::is_external_llama(up) {
            return;
        }
        let mut inner = self.lock();
        let row = inner.rows.entry(up.id).or_default();
        row.then.extend(known);
        row.then.extend(models);
        self.queue(&mut inner, http, FactsKey::server(up), up);
    }

    /// Whether anything about `row` is kept or on its way — what an
    /// invalidation would drop.
    pub fn knows(&self, row: i64) -> bool {
        let inner = self.lock();
        inner.entries.keys().any(|k| k.row == row)
            || inner
                .rows
                .get(&row)
                .is_some_and(|p| !p.queue.is_empty() || p.asking.is_some())
    }

    /// Every key of `row`, sorted: each server first, then its router's
    /// models. Its last answer and whether it is being asked.
    pub fn row(&self, row: i64) -> Vec<KeyState> {
        let inner = self.lock();
        let probe = inner.rows.get(&row);
        let mut out: Vec<KeyState> = inner
            .entries
            .iter()
            .filter(|(k, _)| k.row == row)
            .map(|(k, e)| KeyState {
                key: k.clone(),
                entry: Some(e.clone()),
                probing: probe.is_some_and(|p| p.covers(k)),
            })
            .collect();
        if let Some(p) = probe {
            for k in p.queue.iter().map(|(k, _)| k).chain(&p.asking) {
                if !out.iter().any(|s| &s.key == k) {
                    out.push(KeyState {
                        key: k.clone(),
                        entry: None,
                        probing: true,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// Wait until `row`'s background probing has ended — for tests, which
    /// must not poll. Returns at once when none runs.
    pub async fn settled(&self, row: i64) {
        loop {
            let task = self.lock().rows.get_mut(&row).and_then(|p| p.task.take());
            match task {
                Some(t) => {
                    let _ = t.await;
                }
                None => return,
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }
}

/// Poisoning is recovered from, as `Registry::map` does: a panicking probe
/// task must not wedge every chat send to every external row.
fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ask `row`'s waiting keys one at a time until none is left, under the
/// row's `generation` (`RowProbe::generation`). A server that answers as a
/// router has the models asked about meanwhile queued behind it.
async fn drain(inner: Arc<Mutex<Inner>>, http: reqwest::Client, row: i64, generation: u64) {
    loop {
        let (key, up) = {
            let mut g = lock(&inner);
            // Invalidated: the row is another task's, or none's, now.
            let Some(probe) = g.rows.get_mut(&row).filter(|p| p.generation == generation) else {
                return;
            };
            let Some((key, up)) = probe.queue.pop_front() else {
                probe.running = false;
                probe.asking = None;
                return;
            };
            probe.asking = Some(key.clone());
            (key, up)
        };
        let answer = match &key.model {
            None => ask_server(&http, &up).await,
            Some(model) => ask_model(&http, &up, model).await,
        };
        let mut g = lock(&inner);
        let Inner { entries, rows } = &mut *g;
        let model = key.model.as_deref().unwrap_or("(the server)");
        let Some(probe) = rows.get_mut(&row).filter(|p| p.generation == generation) else {
            // The abort came after the answer did.
            tracing::debug!(
                upstream = %up.name,
                model,
                "GET /props answer dropped: the row was invalidated while it was asked"
            );
            return;
        };
        probe.asking = None;
        if key.model.is_none() {
            let then = std::mem::take(&mut probe.then);
            if matches!(answer, Answer::Router(_)) {
                for m in then {
                    let k = key.model(&m);
                    let kept = entries.get(&k).is_some_and(|e| e.answer.cached());
                    if !kept && !probe.covers(&k) {
                        probe.queue.push_back((k, up.clone()));
                    }
                }
            }
        }
        match &answer {
            Answer::Facts(f) => tracing::debug!(
                upstream = %up.name,
                model,
                build = f.build_info.as_deref().unwrap_or("unknown"),
                "GET /props read"
            ),
            Answer::Router(build) => tracing::debug!(
                upstream = %up.name,
                build = build.as_deref().unwrap_or("unknown"),
                "GET /props: a llama-server router, asked per model"
            ),
            Answer::Unknown(why) | Answer::Retry(why) => tracing::info!(
                upstream = %up.name,
                model,
                "llama-server facts unknown: {why}"
            ),
        }
        entries.insert(
            key,
            FactsEntry {
                answer,
                at: Utc::now(),
            },
        );
    }
}
