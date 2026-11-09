//! Browser API. `scan` is synchronous and takes a while, so call it from a Web Worker.

use wasm_bindgen::prelude::*;

use crate::engine::Scanner;
use crate::vocab::Vocabulary;

#[wasm_bindgen]
pub struct RivenScanner(Scanner);

#[wasm_bindgen]
impl RivenScanner {
    /// Models as `inference.onnx` bytes and `inference.yml` text, vocabulary as the bodies of
    /// warframe.market's `/v2/riven/weapons` and `/v2/riven/attributes`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        det_onnx: Vec<u8>,
        det_yml: String,
        rec_onnx: Vec<u8>,
        rec_yml: String,
        weapons_json: &str,
        attributes_json: &str,
    ) -> Result<RivenScanner, JsError> {
        let vocab = Vocabulary::from_wfm(weapons_json, attributes_json)?;
        Ok(Self(Scanner::new(
            det_onnx, det_yml, rec_onnx, rec_yml, vocab,
        )?))
    }

    /// Scans an encoded image (PNG, JPEG, ...). Returns JSON: `{ riven, lines }`.
    pub fn scan(&self, image: &[u8]) -> Result<String, JsError> {
        let scan = self.0.scan(image)?;
        Ok(serde_json::to_string(&scan)?)
    }
}
