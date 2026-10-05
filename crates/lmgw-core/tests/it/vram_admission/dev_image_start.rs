//! A dev instance's image start on a models dir outside its data dir (the
//! owner's ruling of 2026-10-04 on chat-voice WP11 review m6): it creates no
//! LoRA or upscaler dir there, and it still starts. The writes that are
//! refused outright are `tests/it/dev_models_dir.rs`.

use super::image_pipeline_peak::{acquire_image, add_image_model};
use super::*;

#[tokio::test]
async fn a_dev_image_start_creates_nothing_in_a_shared_models_dir_and_still_starts() {
    let f = fixture(20 * GIB, 6 * GIB, 3 * GIB, 0).await;
    // The fixture's models dir is a temp dir of its own, outside the state's
    // data dir: production's, as a dev copy keeps it.
    f.state.set_dev_for_tests(true);
    add_image_model(&f, "z-image", GIB).await;
    let models = f._models_dir.path();

    let guard = acquire_image(&f, "z-image").await;
    assert_eq!(f.runs().len(), 1, "the start went ahead: {:?}", f.runs());
    assert!(!models.join("loras").exists(), "loras/ was created");
    assert!(!models.join("upscalers").exists(), "upscalers/ was created");
    drop(guard);
}

/// The control: production's start creates both, as before.
#[tokio::test]
async fn a_production_image_start_still_creates_its_dirs() {
    let f = fixture(20 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_image_model(&f, "z-image", GIB).await;
    let guard = acquire_image(&f, "z-image").await;
    let models = f._models_dir.path();
    assert!(models.join("loras").is_dir() && models.join("upscalers").is_dir());
    drop(guard);
}
