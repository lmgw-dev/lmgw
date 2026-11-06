//! §1.6's reads: each engine image the settings name, whether it is here,
//! the bytes of a download of it that runs, and what a download of the
//! others fetches (D62).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lmgw_machine::Facts;

use super::PodmanInfo;
use crate::backends::pull::{self, Bytes, Presence};
use crate::state::AppState;

/// One engine image the settings name (§1.6).
pub(in crate::machine) struct Image {
    pub reference: String,
    pub here: Presence,
    /// While a download of it runs (an `image_pull` job): its bytes so far.
    pub pulling: Option<Bytes>,
    /// What its download fetches, read for a missing image that is not
    /// downloading: the registry manifest's compressed blobs (config and
    /// layers) by short ID, or why they could not be read. The blobs this
    /// machine has already are in [`Images::layers_here`].
    pub blobs: Option<Result<BTreeMap<String, u64>, String>>,
}

/// §1.6's facts: each image, and the compressed layers this machine's image
/// store already has, which a download skips ("Already exists"), as WP10's
/// pull totals it.
pub(in crate::machine) struct Images {
    pub images: Vec<Image>,
    /// Short IDs of the compressed layers in the image store (its
    /// `layers.json`); why they could not be read.
    pub layers_here: Result<BTreeSet<String>, String>,
}

/// §1.6: whether each image is here (`podman image exists`, as a start asks
/// it, WP10), the bytes of a download of it that runs, and for the others
/// what the registry's manifest says a download fetches. The manifest read
/// is the anonymous one WP10's pull totals its bytes with, bounded by that
/// client's own request timeout; a failure is said, never a guess.
pub(super) async fn images(
    state: &Arc<AppState>,
    f: &Arc<dyn Facts>,
    references: Vec<String>,
    info: Option<&PodmanInfo>,
    limit: Duration,
) -> Images {
    use crate::backends::oci::{Platform, PullRef};
    let mut out = Vec::new();
    for reference in references {
        let here = match tokio::time::timeout(limit, pull::presence(state, &reference)).await {
            Ok(p) => p,
            Err(_) => Presence::Unknown(format!(
                "podman image exists did not answer within {} s, the load timeout ({})",
                limit.as_secs(),
                crate::config::LOAD_TIMEOUT_LABEL
            )),
        };
        let mut image = Image {
            reference,
            here,
            pulling: None,
            blobs: None,
        };
        if image.here == Presence::Missing {
            match pull::start_reference(&image.reference) {
                Err(why) => image.blobs = Some(Err(why)),
                Ok(canonical) => {
                    image.pulling = state
                        .jobs
                        .live_by_key(crate::jobs::JobKind::ImagePull, &pull::job_key(&canonical))
                        .and_then(|job| pull::job_bytes(state, job.id));
                    if image.pulling.is_none() {
                        image.blobs = Some(match PullRef::parse(&canonical) {
                            None => Err(format!("{canonical} names no registry manifest")),
                            Some(at) => match state.builds.registry_client() {
                                Ok(client) => client
                                    .platform_blobs(&at, Platform::host())
                                    .await
                                    .map(|b| b.sizes),
                                Err(e) => Err(e),
                            },
                        });
                    }
                }
            }
        }
        out.push(image);
    }
    let wanted = out.iter().any(|i| matches!(i.blobs, Some(Ok(_))));
    let layers_here = match wanted {
        false => Ok(BTreeSet::new()),
        true => store_layers_of(f.clone(), info).await,
    };
    Images {
        images: out,
        layers_here,
    }
}

/// The compressed layers the image store `podman info` named has
/// ([`store_layers`]), read on a blocking thread.
pub(in crate::machine) async fn store_layers_of(
    f: Arc<dyn Facts>,
    info: Option<&PodmanInfo>,
) -> Result<BTreeSet<String>, String> {
    match info {
        Some(PodmanInfo {
            graph_root: Some(root),
            graph_driver: Some(driver),
            ..
        }) => {
            let (root, driver) = (root.clone(), driver.clone());
            tokio::task::spawn_blocking(move || store_layers(&*f, Path::new(&root), &driver))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        }
        _ => Err("podman info names no image store".into()),
    }
}

/// The short IDs of the compressed layers Podman's image store has: the
/// `compressed-diff-digest` of each layer in containers/storage's
/// `<graph_root>/<driver>-layers/layers.json` and `volatile-layers.json`
/// (a pulled layer records the registry blob it came from there; one the
/// store made itself records none). A file that is not there is a store
/// without layers.
fn store_layers(
    f: &dyn Facts,
    graph_root: &Path,
    driver: &str,
) -> Result<BTreeSet<String>, String> {
    let dir = graph_root.join(format!("{driver}-layers"));
    let mut out = BTreeSet::new();
    for name in ["layers.json", "volatile-layers.json"] {
        let path = dir.join(name);
        let bytes = match f.read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let layers: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} is not a layer list: {e}", path.display()))?;
        out.extend(
            layers
                .iter()
                .filter_map(|l| l["compressed-diff-digest"].as_str())
                .map(crate::backends::oci::short_id),
        );
    }
    Ok(out)
}
