//! Print one of lmgw's OpenAPI documents without starting a gateway. Both are
//! pure functions of the crate's own registries, so this is the document
//! `GET /v1/openapi.json` or `GET /api/openapi.json` serves, pretty-printed.
//! The project site's API reference is built from the `v1` one.
//!
//! ```sh
//! cargo run -p lmgw-core --example openapi > v1.json          # the developer document
//! cargo run -p lmgw-core --example openapi -- admin > all.json  # everything
//! ```

use lmgw_core::openapi::{admin_doc, v1_doc};

fn main() {
    let doc = match std::env::args().nth(1).as_deref() {
        None | Some("v1") => v1_doc(),
        Some("admin") => admin_doc(),
        Some(other) => {
            eprintln!("unknown document {other}: v1 (default) or admin");
            std::process::exit(2);
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(doc).expect("a serde_json::Value always serializes")
    );
}
