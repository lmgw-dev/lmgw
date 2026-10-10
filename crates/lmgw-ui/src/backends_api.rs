//! Typed client for the Backends ops (container-builds design §15), plus the
//! three existing ops the page reuses: the settings patch that sets a class's
//! default image, the per-model container apply, and the generic job cancel.
//!
//! Every op is `POST /api/op/<name>` with the §15 argument type as its body
//! and the §15 response type as its answer, both from
//! `lmgw_api_types::builds` — so a field renamed on one side is a compile
//! error here, not a silently empty column.

use lmgw_api_types::builds::{
    BuildCheckMergeArgs, BuildEnv, BuildGetResponse, BuildPromoteArgs, BuildResolveArgs,
    BuildRunArgs, BuildRunLogArgs, BuildRunStarted, BuildSetArgs, BuildSetResponse, BuildSpec,
    BuildUpdatesCheckArgs, BuildUpdatesResponse, BuildVerifyArgs, BuildsGetArgs, BuildsResponse,
    CheckMergeReport, ContainerImageDeleteArgs, ContainerImageDeleteResponse,
    ContainerImagePullArgs, ContainerImagePullStarted, ContainerImagePullStatus,
    ContainerImagePullStatusArgs, ContainerImageTagArgs, ContainerImageTagResponse,
    ContainerImagesArgs, ContainerImagesResponse, ForgePr, ForgePrArgs, ForgePrPage, ForgePrsArgs,
    ForgeRefsArgs, PromoteResponse, RemoteRefsView, ResolvedPreview, RunLogChunk, VerifyReport,
};
use lmgw_api_types::ContainerAnswer;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::api::Result;

async fn op<T: DeserializeOwned, B: Serialize>(name: &str, body: &B) -> Result<T> {
    crate::api::post(format!("/api/op/{name}"), body).await
}

pub async fn builds() -> Result<BuildsResponse> {
    op("builds", &json!({})).await
}

/// One build plus a page of its runs, newest first. `before` is the id of
/// the oldest run already shown.
pub async fn build_get(id: i64, limit: u32, before: Option<i64>) -> Result<BuildGetResponse> {
    op(
        "build_get",
        &BuildsGetArgs {
            id,
            limit: Some(limit),
            before,
        },
    )
    .await
}

pub async fn build_set(args: &BuildSetArgs) -> Result<BuildSetResponse> {
    op("build_set", args).await
}

pub async fn build_resolve(spec: BuildSpec) -> Result<ResolvedPreview> {
    op("build_resolve", &BuildResolveArgs { spec }).await
}

pub async fn build_check_merge(args: &BuildCheckMergeArgs) -> Result<CheckMergeReport> {
    op("build_check_merge", args).await
}

pub async fn build_run(id: i64, rebuild: bool) -> Result<BuildRunStarted> {
    op("build_run", &BuildRunArgs { id, rebuild }).await
}

pub async fn build_run_log(run_id: i64, offset: u64) -> Result<RunLogChunk> {
    op(
        "build_run_log",
        &BuildRunLogArgs {
            run_id,
            offset,
            tail: None,
        },
    )
    .await
}

/// The last `lines` lines of a run's log; the answer's `next_offset`
/// continues from there.
pub async fn build_run_log_tail(run_id: i64, lines: usize) -> Result<RunLogChunk> {
    op(
        "build_run_log",
        &BuildRunLogArgs {
            run_id,
            offset: 0,
            tail: Some(lines),
        },
    )
    .await
}

pub async fn build_promote(run_id: i64) -> Result<PromoteResponse> {
    op("build_promote", &BuildPromoteArgs { run_id }).await
}

pub async fn build_verify(run_id: i64) -> Result<VerifyReport> {
    op("build_verify", &BuildVerifyArgs { run_id }).await
}

pub async fn build_env() -> Result<BuildEnv> {
    op("build_env", &json!({})).await
}

pub async fn forge_refs(repo_url: String) -> Result<RemoteRefsView> {
    op("forge_refs", &ForgeRefsArgs { repo_url }).await
}

pub async fn forge_prs(args: &ForgePrsArgs) -> Result<ForgePrPage> {
    op("forge_prs", args).await
}

pub async fn forge_pr(args: &ForgePrArgs) -> Result<ForgePr> {
    op("forge_pr", args).await
}

/// Every local image of the three engines. `disk: false` skips the footer
/// (`podman system df` and the buildah scan, most of the op's time) for a
/// caller that shows none — the image picker.
pub async fn container_images(disk: bool) -> Result<ContainerImagesResponse> {
    op(
        "container_images",
        &ContainerImagesArgs { engine: None, disk },
    )
    .await
}

/// **Check now** (§8): one build, or every build and every registry image
/// in use when `id` is `None`. Asks the remotes now.
pub async fn build_updates_check(id: Option<i64>) -> Result<BuildUpdatesResponse> {
    op("build_updates_check", &BuildUpdatesCheckArgs { id }).await
}

/// **Pull update** (§8): `podman pull <image>` as an `image_pull` job.
pub async fn container_image_pull(image: String) -> Result<ContainerImagePullStarted> {
    op("container_image_pull", &ContainerImagePullArgs { image }).await
}

/// One pull job as it stands, with what it did once it has ended — the
/// Pull update panel's result and its recreate offer.
pub async fn container_image_pull_status(job_id: i64) -> Result<ContainerImagePullStatus> {
    op(
        "container_image_pull_status",
        &ContainerImagePullStatusArgs { job_id },
    )
    .await
}

/// `force`: delete it even though something uses it — only once the user
/// has seen the users and confirmed.
pub async fn container_image_delete(
    image: String,
    force: bool,
) -> Result<ContainerImageDeleteResponse> {
    op(
        "container_image_delete",
        &ContainerImageDeleteArgs { image, force },
    )
    .await
}

pub async fn container_image_tag(
    args: &ContainerImageTagArgs,
) -> Result<ContainerImageTagResponse> {
    op("container_image_tag", args).await
}

// ---------------------------------------------------------------------------
// Existing ops the page reuses
// ---------------------------------------------------------------------------

/// The settings section that holds a class's default image — the same keys
/// Settings → Runtimes edits (`router.image`, `aux_router.image`,
/// `audio.image`, `image.image`).
pub fn class_section(class: &str) -> Option<&'static str> {
    match class {
        "chat" => Some("router"),
        "aux" => Some("aux_router"),
        "audio" => Some("audio"),
        "image" => Some("image"),
        _ => None,
    }
}

/// Make `image` the default for `class` through `settings_set_full`, the op
/// the Settings page saves with. The class patches are partial: only the
/// `image` key of that one section is sent. The answer's `message` carries
/// the server's note that running models keep the old image until they
/// restart.
pub async fn set_class_default(class: &str, image: &str) -> Result<Value> {
    let Some(patch) = class_default_patch(class, image) else {
        return Err(crate::api::Error::Transport(format!(
            "'{class}' is not a class with a default image"
        )));
    };
    op("settings_set_full", &patch).await
}

/// The `settings_set_full` body for [`set_class_default`]:
/// `{"router": {"image": "<image>"}}` for chat, and so on.
pub fn class_default_patch(class: &str, image: &str) -> Option<Value> {
    let section = class_section(class)?;
    Some(json!({ section: { "image": image } }))
}

/// Recreate one model's container with its current configuration — the
/// per-model `container` op with `action: "apply"`, exactly what a model
/// editor's "Apply to container" sends (never the group apply). A busy
/// refusal is a 200 with `ok: false`; see [`apply_outcome`].
pub async fn model_apply(class: &str, model_id: &str) -> Result<ContainerAnswer> {
    op(
        "container",
        &json!({ "target": class, "model": model_id, "action": "apply" }),
    )
    .await
}

/// A `container` answer as success or the refusal it carries.
pub fn apply_outcome(v: &ContainerAnswer) -> std::result::Result<String, String> {
    let msg = v.message().unwrap_or("applied").to_string();
    if v.ok() {
        Ok(msg)
    } else {
        Err(msg)
    }
}

/// Cancel a background job (a live build run) by its `JobRow.id`.
pub async fn job_cancel(id: i64) -> Result<Value> {
    op("job_cancel", &json!({ "id": id })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_maps_to_the_settings_section_settings_edits() {
        assert_eq!(class_section("chat"), Some("router"));
        assert_eq!(class_section("aux"), Some("aux_router"));
        assert_eq!(class_section("audio"), Some("audio"));
        assert_eq!(class_section("image"), Some("image"));
        assert_eq!(class_section("mail"), None);
    }

    #[test]
    fn a_class_default_patches_only_that_sections_image() {
        assert_eq!(
            class_default_patch("aux", "localhost/lmgw-llama-server:ik-main"),
            Some(json!({"aux_router": {"image": "localhost/lmgw-llama-server:ik-main"}}))
        );
        assert_eq!(class_default_patch("mail", "x"), None);
    }

    #[test]
    fn a_busy_apply_is_a_refusal_even_on_http_200() {
        let answer = |ok: bool, message: &str| {
            serde_json::from_value::<ContainerAnswer>(
                json!({"ok": ok, "class": "chat", "model_id": "m", "message": message}),
            )
            .unwrap()
        };
        assert_eq!(
            apply_outcome(&answer(false, "busy")),
            Err("busy".to_string())
        );
        assert_eq!(
            apply_outcome(&answer(true, "recreated")),
            Ok("recreated".to_string())
        );
    }
}
