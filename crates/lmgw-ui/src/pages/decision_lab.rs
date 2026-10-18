//! The Decision lab (labs-store design §2): ask decision models through
//! `/v1/systemone` from a form — state, images, `choice`, `score` and `noul`
//! questions — compare models, perturb the options, watch a token gauge per
//! header shape, and keep every ask in the lab store.
//!
//! The page posts to `/decision-lab/api/*`, whose cells go in process
//! through the real `/v1/systemone` handler, so hold, admission, fallbacks
//! and the log row are real; its history is the store's, through
//! `pages/lab_history`. The request it shows is `DecisionForm::body`, the
//! function the server builds each cell with.
//!
//! Slice 2 (§3): question sets saved as the lab's presets, set runs over a
//! CSV or JSONL file as a durable job, and each run's own page,
//! `/decision-lab/batches/:id`, with its items, metrics and threshold.

/// The three answer views, and the matrix's compact form and Δ lines.
mod answers;
/// Ask, the cells one at a time, Stop, and the action bar's line.
mod ask;
/// A new set run's file: pasted or uploaded, and its format.
mod batch_input;
/// A set run's items table, read in pages.
mod batch_items;
/// The set runs in the reference panel, newest first.
mod batch_list;
/// A new set run's mapping and its guesses.
mod batch_mapping;
/// The New set run modal: preview, mapping, concurrency, price, Start.
mod batch_new;
/// A new set run's preview: columns, first rows, every problem.
mod batch_preview;
/// A set run's status, progress and controls.
mod batch_status;
/// A set run's page: `/decision-lab/batches/:id`.
mod batch_view;
/// "Also ask on" and Perturb.
mod compare;
/// A question's confusion matrix.
mod confusion;
/// The Examples menu.
mod examples_menu;
/// The form as the page edits it.
mod form_state;
/// The token gauge.
mod gauge;
/// A kept group's card: one cell, or the matrix and its Δ lines.
mod group_card;
/// The head line: the model and its facts.
mod head;
/// The image drop zone and thumbnails.
mod images;
/// What an item's cell says per question: answer, expected, correct.
mod item_cells;
/// A set run's metrics, per question.
mod metrics_view;
/// The page's state and its wiring onto the labs' layout.
mod page;
/// What a set run on its model costs.
mod price_line;
/// A `choice` question's options.
mod question_choice;
/// A `noul` question's criteria.
mod question_noul;
/// A `score` question's levels.
mod question_score;
/// The question builder.
mod questions;
/// The reference panel.
mod reference;
/// A question's reliability diagram.
mod reliability;
/// The Request panel and the override.
mod request;
/// One cell in full: answers, footer, Raw.
mod run_view;
/// The question-set card and each question's threshold line.
mod set_card;
/// Question sets: their state and calls.
mod sets;
/// The reference panel's question sets.
mod sets_panel;
/// The state editor.
mod state_editor;
/// A question's threshold slider and "Use as threshold".
mod threshold;

pub use batch_view::DecisionBatch;
pub use page::DecisionLab;
