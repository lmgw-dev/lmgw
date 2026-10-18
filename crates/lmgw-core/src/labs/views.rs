//! Stored rows as the API's views (`lmgw-api-types::lab_store`): a group
//! with its runs in cell order and its artifacts, each with the `href` its
//! reader reaches the bytes at, and whether it is in flight.

use std::collections::{BTreeMap, HashMap};

use lmgw_api_types::lab_store::{
    LabArtifactRole, LabArtifactView, LabBatchItem, LabCell, LabGroupView, LabKind, LabRunView,
};
use lmgw_api_types::systemone::Ordered;
use serde_json::Value;
use sqlx::SqlitePool;

use super::claims::CellClaims;
use crate::store::{self, DbResult, LabArtifactRow, LabGroupRow, LabRunRow};

/// Where the API serves an artifact's bytes.
pub fn api_href(id: i64) -> String {
    format!("/labs/api/artifacts/{id}")
}

pub fn artifact_view(a: &LabArtifactRow, href: String) -> LabArtifactView {
    LabArtifactView {
        id: a.id,
        run_id: a.run_id,
        role: if a.role == "output" {
            LabArtifactRole::Output
        } else {
            LabArtifactRole::Input
        },
        ord: a.ord,
        name: a.name.clone(),
        mime: a.mime.clone(),
        size: a.size.max(0) as u64,
        sha256: a.sha256.clone(),
        data_url_head: a.data_url_head.clone(),
        href,
        created_at: a.created_at.clone(),
    }
}

/// A stored run with its outputs.
pub fn run_view(r: &LabRunRow, artifacts: Vec<LabArtifactView>) -> LabRunView {
    let d = &r.data;
    LabRunView {
        id: Some(r.id),
        group_id: d.group_id,
        cell: d.cell.max(0) as u32,
        variant: d.variant.clone(),
        endpoint: d.endpoint.clone(),
        model_asked: d.model_asked.clone(),
        answered_by: d.answered_by.clone(),
        fallback_reason: d.fallback_reason.clone(),
        upstream_model: d.upstream_model.clone(),
        http_status: u16::try_from(d.http_status).unwrap_or(0),
        error_code: d.error_code.clone(),
        error_message: d.error_message.clone(),
        request: d.request.clone(),
        response: d.response.clone(),
        response_type: d.response_type.clone(),
        headers: serde_json::from_str::<BTreeMap<String, String>>(&d.headers).unwrap_or_default(),
        input_tokens: d.input_tokens.and_then(|n| u64::try_from(n).ok()),
        output_tokens: d.output_tokens.and_then(|n| u64::try_from(n).ok()),
        latency_ms: d.latency_ms.max(0) as u64,
        created_at: r.created_at.clone(),
        artifacts,
    }
}

/// A stored group's plan; a plan that does not read is no cells.
pub fn plan_of(row: &LabGroupRow) -> Vec<LabCell> {
    serde_json::from_str(&row.plan).unwrap_or_default()
}

/// The groups `rows` as views, in the order given: each with its runs in
/// cell order and its inputs in order, every artifact's `href` from `href`.
pub async fn groups(
    pool: &SqlitePool,
    rows: Vec<LabGroupRow>,
    href: &(dyn Fn(&LabArtifactRow) -> String + Sync),
) -> DbResult<Vec<LabGroupView>> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let runs = store::lab_runs_of(pool, &ids).await?;
    let artifacts = store::lab_artifacts_of(pool, &ids).await?;
    let mut inputs: HashMap<i64, Vec<LabArtifactView>> = HashMap::new();
    let mut outputs: HashMap<i64, Vec<LabArtifactView>> = HashMap::new();
    for a in &artifacts {
        let v = artifact_view(a, href(a));
        match a.run_id {
            Some(run) => outputs.entry(run).or_default().push(v),
            None => inputs.entry(a.group_id).or_default().push(v),
        }
    }
    let mut by_group: HashMap<i64, Vec<LabRunView>> = HashMap::new();
    for r in &runs {
        let outs = outputs.remove(&r.id).unwrap_or_default();
        by_group
            .entry(r.data.group_id)
            .or_default()
            .push(run_view(r, outs));
    }
    Ok(rows
        .into_iter()
        .map(|g| {
            let runs = by_group.remove(&g.id).unwrap_or_default();
            let inputs = inputs.remove(&g.id).unwrap_or_default();
            group_view(g, runs, inputs)
        })
        .collect())
}

/// Stored group `g` as its view, with `runs` in cell order and its
/// `inputs`: what a lab that holds them answers without reading them again.
pub fn group_view(
    g: LabGroupRow,
    runs: Vec<LabRunView>,
    inputs: Vec<LabArtifactView>,
) -> LabGroupView {
    LabGroupView {
        id: g.id,
        lab: LabKind::parse(&g.lab).unwrap_or(LabKind::Decision),
        plan: plan_of(&g),
        item: item_of(&g),
        form: serde_json::from_str(&g.form).unwrap_or(Value::String(g.form)),
        title: g.title,
        override_text: g.override_text,
        created_at: g.created_at,
        runs,
        artifacts: inputs,
        in_flight: false,
    }
}

/// Mark the groups of `views` a cell of which is being answered now.
pub fn mark_in_flight(views: &mut [LabGroupView], claims: &CellClaims) {
    for g in views {
        g.in_flight = claims.live(g.id);
    }
}

/// What makes `g` an item of a set run, when it is one.
pub fn item_of(g: &LabGroupRow) -> Option<LabBatchItem> {
    Some(LabBatchItem {
        batch_id: g.batch_id?,
        item_ix: g.item_ix.unwrap_or_default().max(0) as u64,
        key: g.item_key.clone(),
        expected: g
            .expected
            .as_deref()
            .and_then(|t| serde_json::from_str::<Ordered<String>>(t).ok())
            .unwrap_or_default(),
    })
}
