//! Dashboard widgets. The generic ones (toasts, the custom select, modal,
//! form, popover, section, split, the row menu, the tool picker, the `/`
//! key, ...) live in `lmgw_ui_kit::widgets` and are re-exported here, so no
//! page import changes (`crate::widgets::tool_picker::…` included); what
//! stays is bound to the router, the shell or lmgw's own data.

pub use lmgw_ui_kit::widgets::*;

/// Leaving a page with unsaved edits asks first (the kit's guard, with this
/// router's path and navigation).
pub mod dirty_guard;
/// The one filter row of a long list: search, facets, chips, the count.
pub mod filter_bar;
/// The image picker: a class-image or image-override field over the local
/// images of the class's engine.
pub mod image_picker;
/// The machine check: whether this computer can run local models, and the
/// fix (machine-check design §4.4).
pub mod machine_check;
/// Find a model by its name, sized to the graphics card (find-a-model
/// design §5): the finder its hosts mount.
pub mod model_finder;
/// The frame every `.page` route renders through (head, one scroller, foot).
pub mod page;
/// The agent catalog's config form (agent-catalog §2.6, §6.3): a JSON-schema
/// subset rendered as controls, built out of the widgets in this module.
pub mod schema_form;
/// Tabs that are routes.
pub mod sub_nav;
/// Helpers on the table contract: group rows, the row menu, the "Showing
/// 10 of 55" line.
pub mod table;

pub use dirty_guard::DirtyGuardHost;
pub use filter_bar::{use_slash_focus, Facet, FacetSet, FilterBar};
pub use image_picker::{ImageClass, ImagePicker};
pub use machine_check::{MachineCard, MachineCheck};
pub use page::{Density, PageFrame, PageMode};
pub use sub_nav::{NavTab, SubNav, Tone};
pub use table::{GroupRow, MenuItem, RowMenu, ShowMore};
