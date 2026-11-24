//! What the gateway and its document reader share (document-conversion
//! design §5.2, §14 WP4 review round): the answer's wire form ([`wire`]) and
//! the charset a page is decoded by ([`charset`]). The parsers themselves are
//! the modules of the `lmgw-doc-reader` binary, which the gateway never
//! links.

pub mod charset;
pub mod sandbox;
pub mod wire;
