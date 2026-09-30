//! The HTTP [`Embedder`] (§3): any OpenAI-compatible `/v1/embeddings`.
//!
//! Its reason to exist is crate development. Iterating on retrieval or
//! ingestion inside `quickdoc-core` should not require a second lmgw instance
//! or a second container — point this at the gateway already running on the
//! machine (`http://127.0.0.1:8787/v1`) and the real embedder is available from
//! a plain `cargo test`. In the gateway itself the in-process embedder is the
//! right one: no loopback, and it shares the model-kind gate.
//!
//! Behind the `http` feature so the base crate stays free of an HTTP stack.

use async_trait::async_trait;
use serde_json::json;

use crate::embed::{validate_vector, EmbedIdentity, Embedder};
use crate::error::{QuickdocError, Result};

pub struct HttpEmbedder {
    client: reqwest::Client,
    /// Base URL *including* the `/v1` segment.
    base_url: String,
    api_key: Option<String>,
    /// The name sent as `model` — for lmgw, an alias.
    model: String,
    identity: EmbedIdentity,
}

impl HttpEmbedder {
    /// Connect and learn the vector width by embedding a probe, so the identity
    /// this reports is measured rather than declared.
    ///
    /// `upstream` is the label recorded in [`EmbedIdentity::upstream`]; when the
    /// endpoint is an lmgw, pass the upstream name the corpus is pinned to so
    /// the identity check at query time can succeed.
    pub async fn connect(
        base_url: impl Into<String>,
        api_key: Option<String>,
        upstream: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        let mut e = Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.filter(|k| !k.is_empty()),
            model: model.into(),
            identity: EmbedIdentity::new(upstream, "", 0),
        };
        e.identity = EmbedIdentity::new(
            e.identity.upstream.clone(),
            e.model.clone(),
            e.post(&["quickdoc probe".to_string()])
                .await?
                .first()
                .map(Vec::len)
                .ok_or_else(|| QuickdocError::Embedder("probe returned no vector".into()))?,
        );
        Ok(e)
    }

    async fn post(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut rb = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .json(&json!({ "model": self.model, "input": texts }));
        if let Some(k) = &self.api_key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await.map_err(|e| {
            QuickdocError::Embedder(format!("POST {}/embeddings: {e}", self.base_url))
        })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| QuickdocError::Embedder(e.to_string()))?;
        if !status.is_success() {
            return Err(QuickdocError::Embedder(format!("{status}: {body}")));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| QuickdocError::Embedder(format!("invalid embeddings JSON: {e}")))?;
        let data = v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| QuickdocError::Embedder("embeddings response without 'data'".into()))?;
        data.iter()
            .map(|d| {
                d.get("embedding")
                    .and_then(|e| e.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|f| f.as_f64())
                            .map(|f| f as f32)
                            .collect()
                    })
                    .ok_or_else(|| QuickdocError::Embedder("data entry without 'embedding'".into()))
            })
            .collect()
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    fn identity(&self) -> EmbedIdentity {
        self.identity.clone()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let out = self.post(texts).await?;
        if out.len() != texts.len() {
            return Err(QuickdocError::EmbedCount {
                want: texts.len(),
                got: out.len(),
            });
        }
        for (i, v) in out.iter().enumerate() {
            validate_vector(&self.identity.model, i, v, self.identity.dims)?;
        }
        Ok(out)
    }
}
