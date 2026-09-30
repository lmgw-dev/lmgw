//! The sd-server flag-vocabulary probe (image-generation §2.6).

use std::sync::Arc;

use crate::sdcpp_caps::SdcppCaps;

use super::*;
use crate::runtime::descriptor::ModelRuntime;
use crate::runtime::Class;

impl Registry {
    /// This model's own copy of the descriptor with the image's flag
    /// vocabulary attached, plus whatever went wrong getting it.
    ///
    /// `None` for every class but image (and for an image model whose
    /// vocabulary is already the embedded one), so the common path clones
    /// nothing.
    pub(super) async fn with_sdcpp_caps(
        &self,
        spec: &AcquireSpec<'_>,
    ) -> (Option<ModelRuntime>, Vec<String>) {
        if spec.runtime.class != Class::Image {
            return (None, Vec::new());
        }
        match self
            .sdcpp_caps(
                spec.container_prefix,
                &spec.runtime.image,
                &spec.runtime.extra_run_args,
            )
            .await
        {
            Ok(caps) => {
                let mut rt = spec.runtime.clone();
                rt.sdcpp_caps = Some(caps);
                (Some(rt), Vec::new())
            }
            Err(e) => (
                None,
                vec![format!(
                    "the flag vocabulary of '{}' could not be read ({e}) — the argv was \
                     spelled against the sd-server build lmgw ships with",
                    spec.runtime.image
                )],
            ),
        }
    }

    /// `sd-server --help` for one image, parsed and cached
    /// (image-generation §2.6) — the sd-server twin of [`Self::help_text`].
    ///
    /// Two differences from the llama one, both forced by the binary: the
    /// image's entrypoint is `/sd-cli`, so the server is named explicitly, and
    /// the throwaway run carries `extra_run_args` at all, because sd-server
    /// links `libcuda.so.1` directly and exits 127 on `-h` without the GPU
    /// device (§12.7). Those args are the **caller's resolved** ones — a start
    /// passes the row's own, falling back to the class's (`with_sdcpp_caps`
    /// hands over `spec.runtime.extra_run_args`), and `ops` passes the class's
    /// when there is no row yet. The cache is keyed by image ID alone (see
    /// [`Self::help`] for why the ID), so the first row through an image is the
    /// one whose args ran the probe: they only have to be enough to expose the
    /// card, and a vocabulary is a property of the image either way. A failure
    /// is returned rather than cached: the condition (image not pulled yet,
    /// card taken) is temporary, and the caller degrades to the embedded
    /// vocabulary for this start only.
    ///
    /// The binary is **not** taken from the image's entrypoint, unlike the
    /// llama read: the start path always runs `--entrypoint /sd-server`
    /// ([`crate::runtime::descriptor::SD_SERVER_ENTRYPOINT`]), so that is the only
    /// binary whose vocabulary means anything. The entrypoint is `/sd-cli`
    /// upstream and a shell wrapper in hand-built images — never the server.
    pub async fn sdcpp_caps(
        &self,
        container_prefix: &str,
        image: &str,
        extra_run_args: &[String],
    ) -> Result<Arc<SdcppCaps>, String> {
        let facts = self.image_facts(image).await;
        if let Ok(f) = &facts {
            if let Some(hit) = self
                .sdcpp_help
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&f.id)
                .cloned()
            {
                return Ok(hit);
            }
        }
        let target = facts.as_ref().map_or(image, |f| f.id.as_str());
        let help = self
            .sdcpp_help_read(container_prefix, image, target, extra_run_args)
            .await?;
        let caps = Arc::new(SdcppCaps::parse(&help));
        if caps.is_empty() {
            return Err(format!(
                "`/sd-server --help` from image '{image}' declared no flags at all"
            ));
        }
        if let Some(id) = self.id_after_probe(image, facts).await {
            self.sdcpp_help
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, caps.clone());
        }
        Ok(caps)
    }

    /// The raw `--help` text behind [`Self::sdcpp_caps`], uncached — for a
    /// caller that wants the text itself (a flags surface, a diagnostic).
    pub async fn sdcpp_help_text(
        &self,
        container_prefix: &str,
        image: &str,
        extra_run_args: &[String],
    ) -> Result<String, String> {
        self.sdcpp_help_read(container_prefix, image, image, extra_run_args)
            .await
    }

    /// One throwaway `/sd-server --help` run. `target` is what podman runs —
    /// the resolved image ID where there is one, see [`Self::help_text`] —
    /// and `image` the reference the caller named, which is what the
    /// container name and every message are about.
    async fn sdcpp_help_read(
        &self,
        container_prefix: &str,
        image: &str,
        target: &str,
        extra_run_args: &[String],
    ) -> Result<String, String> {
        let name = format!(
            "{container_prefix}-sdcpp-help-{}",
            crate::runtime::hash6(image)
        );
        let out = self
            .run_throwaway(
                &name,
                target,
                crate::runtime::descriptor::SD_SERVER_ENTRYPOINT,
                // `--help` needs the GPU libraries, never a device.
                &without_gpus(extra_run_args),
                &["--help".to_string()],
            )
            .await?;
        // `--help` exits 0 on this build, but the llama path's lesson holds:
        // trust the output, not the code.
        if out.stdout.len() > 500 {
            return Ok(out.stdout);
        }
        Err(format!(
            "`{} --help` from image '{image}' produced no usable output: {}",
            crate::runtime::descriptor::SD_SERVER_ENTRYPOINT,
            if out.stderr.trim().is_empty() {
                format!("exit {}", out.status)
            } else {
                out.stderr.lines().take(3).collect::<Vec<_>>().join("; ")
            }
        ))
    }
}
