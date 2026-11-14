//! Runs PaddleOCR on a screenshot and parses the result.

use image::DynamicImage;
use image::imageops::FilterType;
use pure_onnx_ocr::{OcrEngine, OcrEngineBuilder, OcrResult};
use serde::Serialize;

use crate::parse::{Line, RerollArea, ScannedRiven, parse_with_layout, rerolls_in};
use crate::vocab::Vocabulary;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ScanError(String);

/// What a scan found: the riven, plus the raw lines for showing or debugging.
#[derive(Clone, Debug, Serialize)]
pub struct Scan {
    pub riven: ScannedRiven,
    pub lines: Vec<Line>,
}

pub struct Scanner {
    engine: OcrEngine,
    vocab: Vocabulary,
}

impl Scanner {
    /// PaddleOCR detection and recognition models: each an `inference.onnx` and its
    /// `inference.yml` (PP-OCRv6 from Hugging Face, `PaddlePaddle/PP-OCRv6_*_onnx`).
    pub fn new(
        det_onnx: Vec<u8>,
        det_yml: String,
        rec_onnx: Vec<u8>,
        rec_yml: String,
        vocab: Vocabulary,
    ) -> Result<Self, ScanError> {
        let engine = OcrEngineBuilder::new()
            .det_model_bytes(det_onnx)
            .det_config_yaml(det_yml)
            .rec_model_bytes(rec_onnx)
            .rec_config_yaml(rec_yml)
            .build()
            .map_err(|e| ScanError(format!("loading the OCR models: {e}")))?;
        Ok(Self { engine, vocab })
    }

    pub fn vocabulary(&self) -> &Vocabulary {
        &self.vocab
    }

    /// Text lines in an encoded image (PNG, JPEG, ...).
    pub fn read_lines(&self, image: &[u8]) -> Result<Vec<Line>, ScanError> {
        self.lines(&decode(image)?)
    }

    fn lines(&self, image: &DynamicImage) -> Result<Vec<Line>, ScanError> {
        let results = self
            .engine
            .run_from_image(image)
            .map_err(|e| ScanError(format!("OCR: {e}")))?;
        Ok(to_lines(results))
    }

    /// Reads the riven on a screenshot of one riven card.
    pub fn scan(&self, image: &[u8]) -> Result<Scan, ScanError> {
        let image = decode(image)?;
        let lines = self.lines(&image)?;
        let (mut riven, area) = parse_with_layout(&self.vocab, &lines);
        // The reroll count is small and sits next to an icon, so the first pass can miss it.
        if let Some(area) = area
            && let Some((count, warning)) = rerolls_in(&self.lines(&enlarged(&image, &area))?)
        {
            riven.rerolls = Some(count);
            riven.warnings.extend(warning);
        }
        Ok(Scan { riven, lines })
    }
}

fn decode(image: &[u8]) -> Result<DynamicImage, ScanError> {
    image::load_from_memory(image).map_err(|e| ScanError(format!("reading the image: {e}")))
}

/// The reroll area is scaled up this much, and put on a margin of this many pixels.
const REROLL_SCALE: u32 = 3;
const REROLL_MARGIN: u32 = 32;

/// The area, cropped and scaled up on a margin, so the detector sees the digits as text.
fn enlarged(image: &DynamicImage, area: &RerollArea) -> DynamicImage {
    let clamp = |v: f32, max: u32| (v.max(0.0) as u32).min(max);
    let x = clamp(area.x, image.width());
    let y = clamp(area.y, image.height());
    let width = clamp(area.width, image.width() - x).max(1);
    let height = clamp(area.height, image.height() - y).max(1);
    let crop = image.crop_imm(x, y, width, height);
    let scaled = crop.resize(
        width * REROLL_SCALE,
        height * REROLL_SCALE,
        FilterType::CatmullRom,
    );
    let mut canvas = DynamicImage::new_rgb8(
        scaled.width() + 2 * REROLL_MARGIN,
        scaled.height() + 2 * REROLL_MARGIN,
    );
    let margin = i64::from(REROLL_MARGIN);
    image::imageops::overlay(&mut canvas, &scaled, margin, margin);
    canvas
}

fn to_lines(results: Vec<OcrResult>) -> Vec<Line> {
    results
        .into_iter()
        .map(|r| {
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for p in r.bounding_box.exterior().points() {
                x0 = x0.min(p.x());
                y0 = y0.min(p.y());
                x1 = x1.max(p.x());
                y1 = y1.max(p.y());
            }
            Line {
                text: r.text,
                confidence: r.confidence,
                x: x0 as f32,
                y: y0 as f32,
                width: (x1 - x0) as f32,
                height: (y1 - y0) as f32,
            }
        })
        .collect()
}
