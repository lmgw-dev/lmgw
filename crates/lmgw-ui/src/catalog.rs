//! The model catalog lives in the shared kit; only its session gate is lmgw's.

pub use lmgw_ui_kit::catalog::*;

/// Install the catalog and fetch it whenever the session opens (kit's
/// `provide_model_catalog`, with this dashboard's session gate). Call once, in `App`.
pub fn provide_model_catalog() {
    lmgw_ui_kit::catalog::provide_model_catalog(crate::session::locked_signal());
}
