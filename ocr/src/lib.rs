//! Reads riven mods from in-game screenshots.
//!
//! [`parse`] turns OCR lines into a [`ScannedRiven`] using the weapon and attribute names in a
//! [`Vocabulary`]. With the `engine` feature, [`Scanner`] also runs the OCR: PaddleOCR PP-OCRv6
//! through `pure-onnx-ocr`, which is pure Rust and runs in the browser. The `wasm` feature
//! exports that to JavaScript, see `scripts/build-web.sh` and `web/`.
//!
//! Scans are suggestions. Show them in an editable form, and check the stats on the server
//! before storing them, since anyone can send any values.

#[cfg(feature = "engine")]
mod engine;
mod parse;
mod text;
mod vocab;
#[cfg(feature = "wasm")]
mod wasm;

#[cfg(feature = "engine")]
pub use engine::{Scan, ScanError, Scanner};
pub use parse::{Line, ScannedAttribute, ScannedRiven, parse};
pub use vocab::{Attribute, RivenName, Unit, Vocabulary, VocabularyError, Weapon};
