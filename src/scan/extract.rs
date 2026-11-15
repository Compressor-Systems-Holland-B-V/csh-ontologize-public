use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xberg::{
    AccelerationConfig, ConcurrencyConfig, ExecutionProviderType, ExtractInput, ExtractionConfig,
    ExtractionMethod, OcrBoundingGeometry, OcrConfig, OcrElement, OcrElementConfig,
    OcrElementLevel, OcrStrategy, OutputFormat, PaddleOcrConfig, PageConfig, PageContent,
};
use xberg_native_pdf::PdfDocument;
use xberg_native_pdf::rendering::{ImageFormat, RenderOptions, render_page};

use crate::config::{self, GpuConfig, GpuProvider, ModelTier, TextFormat};

pub const STAGE_VERSION: u32 = 7;
const XBERG_VERSION: &str = "1.3.10";
pub const OCR_BACKEND: &str = "paddleocr";
const SCANNED: f64 = 0.7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Layer,
    Ocr,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Layer => "layer",
            Method::Ocr => "ocr",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub page_no: u32,
    pub text: String,
    pub method: Method,
    pub ocr_confidence: Option<f64>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Word(pub u32, pub u32, pub f32, pub f32, pub f32, pub f32);

#[derive(Debug, Clone)]
pub struct ExtractSettings {
    xberg: ExtractionConfig,
    visual: ExtractionConfig,
    pub key: String,
}

fn acceleration(gpu: GpuConfig) -> AccelerationConfig {
    AccelerationConfig {
        provider: match gpu.provider {
            GpuProvider::Cpu => ExecutionProviderType::Cpu,
            GpuProvider::Cuda => ExecutionProviderType::Cuda,
        },
        device_id: gpu.device,
    }
}

impl ExtractSettings {
    pub fn new(
        extract: &config::ExtractConfig,
        ocr: &config::OcrConfig,
        gpu: GpuConfig,
    ) -> Result<Self> {
        gpu.usable()?;
        let paddle_ocr_settings = Some(PaddleOcrConfig {
            model_tier: tier_name(ocr.model_tier).to_string(),
            ..PaddleOcrConfig::default()
        });
        let xberg = ExtractionConfig {
            use_cache: false,
            ocr: Some(OcrConfig {
                backend: OCR_BACKEND.to_string(),
                language: ocr.languages.clone(),
                paddle_ocr_settings,
                element_config: Some(OcrElementConfig {
                    include_elements: true,
                    ..OcrElementConfig::default()
                }),
                ..OcrConfig::default()
            }),
            pages: Some(PageConfig {
                extract_pages: true,
                ..PageConfig::default()
            }),
            concurrency: Some(ConcurrencyConfig {
                max_threads: Some(extract.threads),
                ..ConcurrencyConfig::default()
            }),
            max_archive_depth: extract.embedded_depth,
            max_embedded_file_bytes: Some(extract.max_embedded_mb * 1024 * 1024),
            ocr_embedded_images: Some(extract.ocr_embedded_images),
            output_format: match extract.format {
                TextFormat::Markdown => OutputFormat::Markdown,
                TextFormat::Plain => OutputFormat::Plain,
            },
            acceleration: Some(acceleration(gpu)),
            ..ExtractionConfig::default()
        };
        let mut visual = xberg.clone();
        visual.output_format = OutputFormat::Plain;
        if let Some(ocr) = visual.ocr.as_mut() {
            ocr.auto_rotate = true;
        }
        visual.ocr_strategy = OcrStrategy::ScannedPages {
            min_confidence: SCANNED,
        };
        if let Some(element_config) = visual
            .ocr
            .as_mut()
            .and_then(|ocr| ocr.element_config.as_mut())
        {
            element_config.min_level = OcrElementLevel::Word;
        }
        Ok(ExtractSettings {
            xberg,
            visual,
            key: extract_key(extract, ocr),
        })
    }
}

pub fn extract_key(extract: &config::ExtractConfig, ocr: &config::OcrConfig) -> String {
    crate::hash::stage_key(
        "extract",
        STAGE_VERSION,
        &[
            &format!("xberg-{XBERG_VERSION}"),
            OCR_BACKEND,
            tier_name(ocr.model_tier),
            &ocr.languages.join("+"),
            extract.format.as_str(),
            &format!(
                "embedded-{}-{}mb-ocr-{}",
                extract.embedded_depth, extract.max_embedded_mb, extract.ocr_embedded_images
            ),
            "visual-words-text-scanned-pages-auto-rotate",
        ],
    )
}

pub fn tier_name(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Small => "small",
        ModelTier::Medium => "medium",
    }
}

pub async fn extract_pages(path: &Path, settings: &ExtractSettings) -> Result<Vec<Page>> {
    let uri = path.to_string_lossy().into_owned();
    if uri.contains("://") {
        bail!("not a file path: {uri}");
    }
    let mime = xberg::detect_mime_type(uri.clone(), true).unwrap_or_default();
    let visual = mime == PDF_MIME || mime.starts_with("image/");
    let config = if visual {
        &settings.visual
    } else {
        &settings.xberg
    };
    let input = ExtractInput::from_uri(uri);
    let result = xberg::extract(input, config)
        .await
        .with_context(|| format!("extraction failed: {}", path.display()))?;
    if result.results.is_empty() {
        let reasons: Vec<String> = result.errors.iter().map(|e| format!("{e:?}")).collect();
        bail!(
            "no text extracted from {}: {}",
            path.display(),
            reasons.join("; ")
        );
    }
    let pdf = (visual && mime == PDF_MIME)
        .then(|| std::fs::read(path).ok())
        .flatten()
        .and_then(|bytes| PdfDocument::from_bytes(bytes).ok());
    let mut pages = Vec::new();
    for (index, document) in result.results.into_iter().enumerate() {
        let offset = pages.len() as u32;
        let elements = document.ocr_elements.unwrap_or_default();
        let frames = ocr_frames(|key| document.metadata.additional.get(key));
        let picture = picture(&document.metadata).map(|picture| Picture {
            turned: auto_rotation(|key| document.metadata.additional.get(key)),
            ..picture
        });
        let mut document_pages = document_pages(
            document.pages,
            document.content,
            document.extraction_method,
            &elements,
        );
        if visual {
            let layer = pdf
                .as_ref()
                .filter(|_| index == 0 && document.mime_type == PDF_MIME);
            add_words(&mut document_pages, &elements, &frames, layer, picture);
        }
        pages.extend(document_pages.into_iter().map(|mut page| {
            page.page_no += offset;
            page
        }));
    }
    Ok(pages)
}

const PDF_MIME: &str = "application/pdf";

#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
    page_no: Option<u32>,
    width: f32,
    height: f32,
}

fn ocr_frames<'a>(additional: impl Fn(&str) -> Option<&'a Value>) -> Vec<Frame> {
    let listed: Vec<Frame> = additional("ocr_page_coordinate_frames")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|frame| {
            Some(Frame {
                page_no: Some(frame.get("page_number")?.as_u64()? as u32),
                width: frame.get("width")?.as_f64()? as f32,
                height: frame.get("height")?.as_f64()? as f32,
            })
        })
        .collect();
    if !listed.is_empty() {
        return listed;
    }
    let dimension = |key: &str| additional(key).and_then(Value::as_f64);
    match (
        dimension("ocr_processed_image_width"),
        dimension("ocr_processed_image_height"),
    ) {
        (Some(width), Some(height)) if width > 0.0 && height > 0.0 => vec![Frame {
            page_no: None,
            width: width as f32,
            height: height as f32,
        }],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Area {
    llx: f32,
    lly: f32,
    width: f32,
    height: f32,
}

impl Area {
    fn of(x0: f32, y0: f32, x1: f32, y1: f32) -> Area {
        Area {
            llx: x0.min(x1),
            lly: y0.min(y1),
            width: (x1 - x0).abs(),
            height: (y1 - y0).abs(),
        }
    }

    fn within(self, outer: Area) -> Area {
        let llx = self.llx.max(outer.llx);
        let lly = self.lly.max(outer.lly);
        Area {
            llx,
            lly,
            width: (self.llx + self.width).min(outer.llx + outer.width) - llx,
            height: (self.lly + self.height).min(outer.lly + outer.height) - lly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PdfPage {
    media: Area,
    shown: Area,
    rotate: i32,
}

impl PdfPage {
    fn read(pdf: &PdfDocument, index: usize) -> Option<PdfPage> {
        let info = pdf.get_page_info(index).ok()?;
        let rect =
            |r: xberg_native_pdf::geometry::Rect| Area::of(r.x, r.y, r.x + r.width, r.y + r.height);
        let media = rect(info.media_box);
        let shown = info.crop_box.map_or(media, |crop| rect(crop).within(media));
        let page = PdfPage {
            media,
            shown,
            rotate: pdf.get_page_rotation(index).unwrap_or(0),
        };
        (shown.width > 0.0 && shown.height > 0.0 && media.width > 0.0 && media.height > 0.0)
            .then_some(page)
    }

    fn shown(&self) -> (f32, f32) {
        match self.rotate {
            90 | 270 => (self.shown.height, self.shown.width),
            _ => (self.shown.width, self.shown.height),
        }
    }

    fn place(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> [f32; 4] {
        let area = self.shown;
        let u = |x: f32| (x - area.llx) / area.width;
        let v = |y: f32| 1.0 - (y - area.lly) / area.height;
        let (a, b) = (
            turn(u(x0), v(y0), self.rotate),
            turn(u(x1), v(y1), self.rotate),
        );
        [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]
    }

    fn place_word(&self, word: &xberg_native_pdf::layout::Word) -> Option<[f32; 4]> {
        let b = word.bbox;
        let size = word.avg_font_size;
        let quarter = |degrees: f32| (word.rotation_degrees - degrees).abs() < 0.5;
        let [x0, y0, x1, y1] = match self.rotate {
            0 | 90 | 270 if quarter(0.0) => [b.x, b.y, b.x + b.width, b.y + b.height],
            90 if quarter(90.0) => {
                let top = b.y + b.height;
                self.unturn([b.x, top, b.x + b.width, top + size])
            }
            270 if quarter(-90.0) => {
                let left = b.x + size;
                self.unturn([left, b.y, left + b.width, b.y + size])
            }
            _ => return None,
        };
        Some(self.place(x0, y0, x1, y1))
    }

    fn unturn(&self, [x0, y0, x1, y1]: [f32; 4]) -> [f32; 4] {
        let m = self.media;
        let point = |x: f32, y: f32| {
            let (dx, dy) = (x - m.llx, y - m.lly);
            match self.rotate {
                90 => (m.llx + m.width - dy, m.lly + dx),
                _ => (m.llx + dy, m.lly + m.height - dx),
            }
        };
        let (a, b) = (point(x0, y0), point(x1, y1));
        [a.0, a.1, b.0, b.1]
    }
}

fn turn(u: f32, v: f32, rotate: i32) -> (f32, f32) {
    match rotate {
        90 => (1.0 - v, u),
        180 => (1.0 - u, 1.0 - v),
        270 => (v, 1.0 - u),
        _ => (u, v),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Picture {
    width: f32,
    height: f32,
    orientation: u8,
    turned: i32,
}

impl Picture {
    fn upright(&self) -> (f32, f32) {
        match self.orientation {
            5..=8 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }

    fn place(&self, [u0, v0, u1, v1]: [f32; 4]) -> [f32; 4] {
        let back = (360 - self.turned).rem_euclid(360);
        let stored = |u: f32, v: f32| turn(u, v, back);
        let (a, b) = (stored(u0, v0), stored(u1, v1));
        let (a, b) = (self.orient(a.0, a.1), self.orient(b.0, b.1));
        [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]
    }

    fn orient(&self, u: f32, v: f32) -> (f32, f32) {
        match self.orientation {
            2 => (1.0 - u, v),
            3 => turn(u, v, 180),
            4 => (u, 1.0 - v),
            5 => (v, u),
            6 => turn(u, v, 90),
            7 => (1.0 - v, 1.0 - u),
            8 => turn(u, v, 270),
            _ => (u, v),
        }
    }
}

fn picture(metadata: &xberg::Metadata) -> Option<Picture> {
    let Some(xberg::FormatMetadata::Image(image)) = &metadata.format else {
        return None;
    };
    Some(Picture {
        width: image.width as f32,
        height: image.height as f32,
        orientation: image
            .exif
            .get("Orientation")
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(1),
        turned: 0,
    })
}

fn auto_rotation<'a>(additional: impl Fn(&str) -> Option<&'a Value>) -> i32 {
    if additional("auto_rotated").and_then(Value::as_bool) != Some(true) {
        return 0;
    }
    additional("orientation_degrees")
        .and_then(Value::as_i64)
        .map_or(0, |degrees| (360 - degrees as i32).rem_euclid(360))
}

const INK_DPI: u32 = 72;
const INK_MARGIN: f32 = 1.2;
const INK_ABOVE_PAGE: f32 = 1.5;

fn shown_turn(
    pdf: &PdfDocument,
    index: usize,
    page: &PdfPage,
    raster: (f32, f32),
    words: &[[f32; 4]],
) -> Option<i32> {
    if page.shown != page.media || words.is_empty() {
        return None;
    }
    let mut options = RenderOptions::default();
    options.dpi = INK_DPI;
    options.format = ImageFormat::RawRgba8;
    let image = render_page(pdf, index, &options).ok()?;
    let (width, height) = (image.width as usize, image.height as usize);
    let dark: Vec<bool> = image
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2]) < 384)
        .collect();
    if dark.len() != width * height {
        return None;
    }
    let page_ink = dark.iter().filter(|&&one| one).count() as f32 / dark.len().max(1) as f32;
    let ink = |turned: i32| {
        let shares: Vec<f32> = words
            .iter()
            .filter_map(|&word| {
                let [x0, y0, x1, y1] = turned_box(word, turned);
                let xs = (x0 * width as f32) as usize..((x1 * width as f32) as usize).min(width);
                let ys = (y0 * height as f32) as usize..((y1 * height as f32) as usize).min(height);
                let area = xs.len() * ys.len();
                let inked = ys
                    .flat_map(|y| xs.clone().map(move |x| y * width + x))
                    .filter(|&at| dark[at])
                    .count();
                (area > 0).then(|| inked as f32 / area as f32)
            })
            .collect();
        shares.iter().sum::<f32>() / shares.len().max(1) as f32
    };
    let (shown_width, shown_height) = page.shown();
    let square = |ratio: f32| (ratio - 1.0).abs() < 0.05;
    let either = square(raster.0 / raster.1) || square(shown_width / shown_height);
    let quarter = (raster.0 > raster.1) != (shown_width > shown_height);
    let mut scored: Vec<(f32, i32)> = [0, 90, 180, 270]
        .into_iter()
        .filter(|turned| either || (turned % 180 == 90) == quarter)
        .map(|turned| (ink(turned), turned))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let ((best, turned), (second, _)) = (scored[0], scored[1]);
    (best >= INK_MARGIN * second && best >= INK_ABOVE_PAGE * page_ink).then_some(turned)
}

fn turned_box([u0, v0, u1, v1]: [f32; 4], degrees: i32) -> [f32; 4] {
    let (a, b) = (turn(u0, v0, degrees), turn(u1, v1, degrees));
    [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]
}

fn pixel_box(geometry: &OcrBoundingGeometry) -> [f32; 4] {
    match geometry {
        OcrBoundingGeometry::Rectangle {
            left,
            top,
            width,
            height,
        } => [
            *left as f32,
            *top as f32,
            (left + width) as f32,
            (top + height) as f32,
        ],
        OcrBoundingGeometry::Quadrilateral { points } => {
            let xs = || points.iter().map(|point| point.x as f32);
            let ys = || points.iter().map(|point| point.y as f32);
            [
                xs().fold(f32::INFINITY, f32::min),
                ys().fold(f32::INFINITY, f32::min),
                xs().fold(f32::NEG_INFINITY, f32::max),
                ys().fold(f32::NEG_INFINITY, f32::max),
            ]
        }
    }
}

fn page_frame(frames: &[Frame], page_no: u32, single_page: bool) -> Option<(f32, f32)> {
    frames
        .iter()
        .find(|frame| frame.page_no == Some(page_no))
        .or_else(|| {
            frames
                .iter()
                .find(|frame| single_page && frame.page_no.is_none())
        })
        .map(|frame| (frame.width, frame.height))
}

fn add_words(
    pages: &mut [Page],
    elements: &[OcrElement],
    frames: &[Frame],
    pdf: Option<&PdfDocument>,
    picture: Option<Picture>,
) {
    let single_page = pages.len() == 1;
    for page in pages.iter_mut() {
        let index = page.page_no.saturating_sub(1) as usize;
        let geometry = pdf.and_then(|pdf| PdfPage::read(pdf, index));
        let found: Vec<Found> = match page.method {
            Method::Ocr => {
                let Some((width, height)) = page_frame(frames, page.page_no, single_page) else {
                    continue;
                };
                let (shown_width, shown_height) = match (geometry, picture) {
                    (Some(geometry), _) => geometry.shown(),
                    (None, Some(picture)) => picture.upright(),
                    (None, None) => (width, height),
                };
                page.width = Some(shown_width);
                page.height = Some(shown_height);
                let words: Vec<(String, [f32; 4])> = elements
                    .iter()
                    .filter(|e| e.level == OcrElementLevel::Word && e.page_number == page.page_no)
                    .map(|e| {
                        let [x0, y0, x1, y1] = pixel_box(&e.geometry);
                        (
                            e.text.clone(),
                            [x0 / width, y0 / height, x1 / width, y1 / height],
                        )
                    })
                    .collect();
                let turned = match (geometry, pdf) {
                    (Some(geometry), Some(pdf)) => {
                        let boxes: Vec<[f32; 4]> = words.iter().map(|(_, b)| *b).collect();
                        match shown_turn(pdf, index, &geometry, (width, height), &boxes) {
                            Some(turned) => Some(turned),
                            None => continue,
                        }
                    }
                    _ => None,
                };
                words
                    .into_iter()
                    .map(|(text, raster)| {
                        let placed = match (turned, picture) {
                            (Some(turned), _) => turned_box(raster, turned),
                            (None, Some(picture)) => picture.place(raster),
                            (None, None) => raster,
                        };
                        (text, raster, Some(placed))
                    })
                    .collect()
            }
            Method::Layer => {
                let (Some(pdf), Some(geometry)) = (pdf, geometry) else {
                    continue;
                };
                let (shown_width, shown_height) = geometry.shown();
                page.width = Some(shown_width);
                page.height = Some(shown_height);
                pdf.extract_words(index)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|word| {
                        let b = word.bbox;
                        let source = [b.x, b.y, b.x + b.width, b.y + b.height];
                        let placed = geometry.place_word(&word);
                        (word.text, source, placed)
                    })
                    .collect()
            }
        };
        if found.iter().any(|(text, _, _)| !text.trim().is_empty()) {
            (page.text, page.words) = page_text(&found);
        }
    }
}

type Found = (String, [f32; 4], Option<[f32; 4]>);

fn page_text(found: &[Found]) -> (String, Vec<Word>) {
    let mut text = String::new();
    let mut count = 0u32;
    let mut words = Vec::new();
    let mut previous: Option<[f32; 4]> = None;
    for (word, source, placed) in found {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        if let Some(last) = previous {
            text.push(if same_line(last, *source) { ' ' } else { '\n' });
            count += 1;
        }
        let start = count;
        text.push_str(word);
        count += word.chars().count() as u32;
        if let Some([x0, y0, x1, y1]) = placed {
            words.push(Word(
                start,
                count,
                unit(*x0),
                unit(*y0),
                unit(*x1),
                unit(*y1),
            ));
        }
        previous = Some(*source);
    }
    (text, words)
}

fn same_line(last: [f32; 4], next: [f32; 4]) -> bool {
    let middle = (next[1] + next[3]) / 2.0;
    (last[1].min(last[3])..=last[1].max(last[3])).contains(&middle) && next[0] >= last[0]
}

fn unit(value: f32) -> f32 {
    (value.clamp(0.0, 1.0) * 10_000.0).round() / 10_000.0
}

fn document_pages(
    pages: Option<Vec<PageContent>>,
    content: String,
    extraction_method: Option<ExtractionMethod>,
    elements: &[OcrElement],
) -> Vec<Page> {
    match pages {
        Some(pages) if !pages.is_empty() => pages
            .into_iter()
            .map(|page| to_page(page, elements))
            .collect(),
        _ => {
            let method = match extraction_method {
                Some(ExtractionMethod::Ocr) => Method::Ocr,
                _ => Method::Layer,
            };
            let ocr_confidence = match method {
                Method::Ocr => mean_recognition(elements, None),
                Method::Layer => None,
            };
            vec![Page {
                page_no: 1,
                text: content,
                method,
                ocr_confidence,
                width: None,
                height: None,
                words: Vec::new(),
            }]
        }
    }
}

fn to_page(page: PageContent, elements: &[OcrElement]) -> Page {
    let (method, ocr_confidence) = match &page.ocr_confidence {
        None => (Method::Layer, None),
        Some(confidence) => (
            Method::Ocr,
            confidence
                .score
                .or_else(|| mean_recognition(elements, Some(page.page_number))),
        ),
    };
    Page {
        page_no: page.page_number,
        text: page.content,
        method,
        ocr_confidence,
        width: None,
        height: None,
        words: Vec::new(),
    }
}

fn mean_recognition(elements: &[OcrElement], page_no: Option<u32>) -> Option<f64> {
    let scores: Vec<f64> = elements
        .iter()
        .filter(|element| element.level == OcrElementLevel::Line)
        .filter(|element| page_no.is_none_or(|n| element.page_number == n))
        .map(|element| element.confidence.recognition)
        .collect();
    if scores.is_empty() {
        return None;
    }
    Some(scores.iter().sum::<f64>() / scores.len() as f64)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use xberg::{OcrBoundingGeometry, OcrConfidence, OcrElementLevel, PageOcrConfidence};

    use super::*;

    #[test]
    fn xberg_version_matches_cargo_lock() {
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock")).unwrap();
        for name in ["xberg", "xberg-native-pdf"] {
            let version = lock
                .split("[[package]]")
                .find(|block| block.contains(&format!("\nname = \"{name}\"\n")))
                .and_then(|block| block.lines().find_map(|l| l.strip_prefix("version = ")))
                .unwrap();
            assert_eq!(version.trim_matches('"'), XBERG_VERSION, "{name}");
        }
    }

    fn page_content(page_number: u32, ocr: Option<PageOcrConfidence>) -> PageContent {
        PageContent {
            page_number,
            content: format!("page {page_number}"),
            tables: Vec::new(),
            image_indices: Vec::new(),
            image_preprocessing: None,
            hierarchy: None,
            is_blank: None,
            layout_regions: None,
            speaker_notes: None,
            section_name: None,
            sheet_name: None,
            ocr_confidence: ocr,
            native_content: None,
        }
    }

    fn element(page_number: u32, recognition: f64) -> OcrElement {
        OcrElement {
            text: "word".to_string(),
            geometry: OcrBoundingGeometry::Rectangle {
                left: 0,
                top: 0,
                width: 1,
                height: 1,
            },
            confidence: OcrConfidence {
                detection: None,
                recognition,
            },
            level: OcrElementLevel::Line,
            rotation: None,
            page_number,
            parent_id: None,
            backend_metadata: HashMap::new(),
        }
    }

    fn ocr_without_score() -> Option<PageOcrConfidence> {
        Some(PageOcrConfidence {
            score: None,
            word_count: 0,
            backend: "paddle-ocr".to_string(),
        })
    }

    #[test]
    fn extract_key_changes_with_ocr_settings() {
        let config = config::load(None).unwrap();
        let base = extract_key(&config.extract, &config.ocr);
        let mut ocr = config.ocr.clone();
        ocr.model_tier = ModelTier::Medium;
        assert_ne!(base, extract_key(&config.extract, &ocr));
        let mut ocr = config.ocr.clone();
        ocr.languages.push("nl".to_string());
        assert_ne!(base, extract_key(&config.extract, &ocr));
        let mut extract = config.extract.clone();
        extract.format = TextFormat::Plain;
        assert_ne!(base, extract_key(&extract, &config.ocr));
        let mut extract = config.extract.clone();
        extract.embedded_depth = 1;
        assert_ne!(base, extract_key(&extract, &config.ocr));
        let mut extract = config.extract.clone();
        extract.ocr_embedded_images = true;
        assert_ne!(base, extract_key(&extract, &config.ocr));
        let mut extract = config.extract.clone();
        extract.workers += 1;
        extract.worker_memory_mb += 1;
        assert_eq!(base, extract_key(&extract, &config.ocr));
        let mut extract = config.extract.clone();
        extract.threads += 1;
        assert_eq!(base, extract_key(&extract, &config.ocr));
    }

    #[test]
    fn page_without_ocr_is_layer() {
        let page = to_page(page_content(1, None), &[element(1, 0.9)]);
        assert_eq!(page.method, Method::Layer);
        assert_eq!(page.ocr_confidence, None);
    }

    #[test]
    fn ocr_page_takes_mean_of_its_own_elements() {
        let elements = [element(1, 0.5), element(2, 0.8), element(2, 0.6)];
        let page = to_page(page_content(2, ocr_without_score()), &elements);
        assert_eq!(page.method, Method::Ocr);
        let confidence = page.ocr_confidence.unwrap();
        assert!((confidence - 0.7).abs() < 1e-9, "{confidence}");
    }

    #[test]
    fn backend_score_wins_over_elements() {
        let ocr = Some(PageOcrConfidence {
            score: Some(0.42),
            word_count: 10,
            backend: "paddle-ocr".to_string(),
        });
        let page = to_page(page_content(1, ocr), &[element(1, 0.9)]);
        assert_eq!(page.ocr_confidence, Some(0.42));
    }

    #[test]
    fn ocr_page_without_elements_has_no_confidence() {
        let page = to_page(page_content(1, ocr_without_score()), &[]);
        assert_eq!(page.method, Method::Ocr);
        assert_eq!(page.ocr_confidence, None);
    }

    #[test]
    fn document_without_pages_becomes_one_page() {
        let pages = document_pages(
            None,
            "mail body".to_string(),
            Some(ExtractionMethod::Native),
            &[],
        );
        assert_eq!(
            pages,
            [Page {
                page_no: 1,
                text: "mail body".to_string(),
                method: Method::Layer,
                ocr_confidence: None,
                width: None,
                height: None,
                words: Vec::new(),
            }]
        );
    }

    #[test]
    fn ocr_document_without_pages_uses_all_elements() {
        let pages = document_pages(
            Some(Vec::new()),
            "scan".to_string(),
            Some(ExtractionMethod::Ocr),
            &[element(1, 0.4), element(1, 0.6)],
        );
        assert_eq!(pages[0].method, Method::Ocr);
        assert!((pages[0].ocr_confidence.unwrap() - 0.5).abs() < 1e-9);
    }

    fn found(words: &[(&str, f32)]) -> Vec<Found> {
        words
            .iter()
            .map(|&(word, y)| {
                let source = [0.1, y, 0.2, y + 0.05];
                (word.to_string(), source, Some(source))
            })
            .collect()
    }

    #[test]
    fn page_text_is_built_from_the_words() {
        let (text, words) = page_text(&found(&[("Ærø", 0.1), ("pump", 0.1), ("P-101", 0.3)]));
        assert_eq!(text, "Ærø pump\nP-101");
        let chars: Vec<char> = text.chars().collect();
        let slices: Vec<String> = words
            .iter()
            .map(|w| chars[w.0 as usize..w.1 as usize].iter().collect())
            .collect();
        assert_eq!(slices, ["Ærø", "pump", "P-101"]);
    }

    #[test]
    fn a_word_left_of_the_last_starts_a_line() {
        let mut words = found(&[("one", 0.1), ("two", 0.1)]);
        words[1].1[0] = 0.0;
        assert_eq!(page_text(&words).0, "one\ntwo");
    }

    #[test]
    fn a_word_without_a_box_stays_in_the_text() {
        let mut words = found(&[("Jan", 0.1), ("an", 0.1), ("", 0.1)]);
        words[0].2 = None;
        let (text, placed) = page_text(&words);
        assert_eq!(text, "Jan an");
        assert_eq!(placed, [Word(4, 6, 0.1, 0.1, 0.2, 0.15)]);
    }

    #[test]
    fn boxes_clamp_and_round() {
        let words = [("a".to_string(), [0.0; 4], Some([-0.2, 0.123456, 1.3, 0.5]))];
        assert_eq!(page_text(&words).1, [Word(0, 1, 0.0, 0.1235, 1.0, 0.5)]);
    }

    fn page(rotate: i32) -> PdfPage {
        let area = Area::of(0.0, 0.0, 200.0, 100.0);
        PdfPage {
            media: area,
            shown: area,
            rotate,
        }
    }

    fn close(a: [f32; 4], b: [f32; 4]) {
        assert!(
            a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-3),
            "{a:?} {b:?}"
        );
    }

    #[test]
    fn layer_box_flips_y_from_the_baseline() {
        close(page(0).place(20.0, 50.0, 60.0, 60.0), [0.1, 0.4, 0.3, 0.5]);
        let area = Area::of(10.0, 20.0, 210.0, 120.0);
        let shifted = PdfPage {
            media: area,
            shown: area,
            rotate: 0,
        };
        close(shifted.place(30.0, 70.0, 70.0, 80.0), [0.1, 0.4, 0.3, 0.5]);
    }

    #[test]
    fn layer_box_turns_with_the_page() {
        close(page(90).place(20.0, 50.0, 60.0, 60.0), [0.5, 0.1, 0.6, 0.3]);
        close(
            page(180).place(20.0, 50.0, 60.0, 60.0),
            [0.7, 0.5, 0.9, 0.6],
        );
        close(
            page(270).place(20.0, 50.0, 60.0, 60.0),
            [0.4, 0.7, 0.5, 0.9],
        );
        assert_eq!(page(90).shown(), (100.0, 200.0));
        assert_eq!(page(180).shown(), (200.0, 100.0));
    }

    #[test]
    fn crop_box_inside_the_media_box_is_the_page() {
        let crop = Area::of(-50.0, 10.0, 100.0, 60.0).within(Area::of(0.0, 0.0, 200.0, 100.0));
        assert_eq!(crop, Area::of(0.0, 10.0, 100.0, 60.0));
        let cropped = PdfPage {
            shown: crop,
            ..page(0)
        };
        close(cropped.place(10.0, 30.0, 30.0, 40.0), [0.1, 0.4, 0.3, 0.6]);
        assert_eq!(cropped.shown(), (100.0, 50.0));
    }

    #[test]
    fn a_box_turns_clockwise_with_the_page() {
        let raster = [0.1, 0.4, 0.3, 0.5];
        close(turned_box(raster, 0), raster);
        close(turned_box(raster, 90), [0.5, 0.1, 0.6, 0.3]);
        close(turned_box(raster, 270), [0.4, 0.7, 0.5, 0.9]);
    }

    fn picture(orientation: u8) -> Picture {
        Picture {
            width: 640.0,
            height: 480.0,
            orientation,
            turned: 0,
        }
    }

    #[test]
    fn exif_orientation_turns_the_boxes_upright() {
        let raw = [0.1, 0.2, 0.3, 0.4];
        let cases = [
            (1, [0.1, 0.2, 0.3, 0.4]),
            (2, [0.7, 0.2, 0.9, 0.4]),
            (3, [0.7, 0.6, 0.9, 0.8]),
            (4, [0.1, 0.6, 0.3, 0.8]),
            (5, [0.2, 0.1, 0.4, 0.3]),
            (6, [0.6, 0.1, 0.8, 0.3]),
            (7, [0.6, 0.7, 0.8, 0.9]),
            (8, [0.2, 0.7, 0.4, 0.9]),
        ];
        for (orientation, upright) in cases {
            close(picture(orientation).place(raw), upright);
        }
        assert_eq!(picture(1).upright(), (640.0, 480.0));
        assert_eq!(picture(6).upright(), (480.0, 640.0));
        assert_eq!(picture(3).upright(), (640.0, 480.0));
    }

    #[test]
    fn document_frame_serves_a_single_page_only() {
        let values: HashMap<&str, Value> = [
            ("ocr_processed_image_width", Value::from(1240)),
            ("ocr_processed_image_height", Value::from(1754)),
        ]
        .into_iter()
        .collect();
        let frames = ocr_frames(|key| values.get(key));
        assert_eq!(page_frame(&frames, 1, true), Some((1240.0, 1754.0)));
        assert_eq!(page_frame(&frames, 1, false), None);
        assert_eq!(page_frame(&frames, 2, false), None);
    }

    #[test]
    fn listed_frames_win_per_page() {
        let values: HashMap<&str, Value> = [
            ("ocr_processed_image_width", Value::from(100)),
            ("ocr_processed_image_height", Value::from(100)),
            (
                "ocr_page_coordinate_frames",
                serde_json::json!([{"page_number": 2, "width": 1240, "height": 600, "unit": "pixel"}]),
            ),
        ]
        .into_iter()
        .collect();
        let frames = ocr_frames(|key| values.get(key));
        assert_eq!(page_frame(&frames, 2, false), Some((1240.0, 600.0)));
        assert_eq!(page_frame(&frames, 1, false), None);
        assert_eq!(page_frame(&frames, 1, true), None);
    }

    fn ocr_word(
        page_number: u32,
        text: &str,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
    ) -> OcrElement {
        let mut word = element(page_number, 0.9);
        word.level = OcrElementLevel::Word;
        word.text = text.to_string();
        word.geometry = OcrBoundingGeometry::Rectangle {
            left,
            top,
            width,
            height,
        };
        word
    }

    fn ocr_page(page_number: u32, text: &str) -> Page {
        let mut page = to_page(page_content(page_number, ocr_without_score()), &[]);
        page.text = text.to_string();
        page
    }

    #[test]
    fn ocr_words_fill_the_page() {
        let frames = [Frame {
            page_no: None,
            width: 620.0,
            height: 300.0,
        }];
        let mut pages = [ocr_page(1, "Valve")];
        let elements = [ocr_word(1, "Valve", 62, 30, 124, 60), element(1, 0.5)];
        add_words(&mut pages, &elements, &frames, None, None);
        assert_eq!(pages[0].words, [Word(0, 5, 0.1, 0.1, 0.3, 0.3)]);
        assert_eq!(
            (pages[0].width, pages[0].height),
            (Some(620.0), Some(300.0))
        );
    }

    #[test]
    fn ocr_page_takes_only_its_own_frame() {
        let frames = [
            Frame {
                page_no: Some(1),
                width: 600.0,
                height: 300.0,
            },
            Frame {
                page_no: Some(2),
                width: 1200.0,
                height: 600.0,
            },
        ];
        let mut pages = [
            ocr_page(1, "Valve"),
            ocr_page(2, "Pump"),
            ocr_page(3, "Seal"),
        ];
        let elements = [
            ocr_word(1, "Valve", 60, 30, 120, 60),
            ocr_word(2, "Pump", 120, 60, 240, 120),
            ocr_word(3, "Seal", 60, 30, 120, 60),
        ];
        add_words(&mut pages, &elements, &frames, None, None);
        assert_eq!(pages[0].words, [Word(0, 5, 0.1, 0.1, 0.3, 0.3)]);
        assert_eq!(pages[1].words, [Word(0, 4, 0.1, 0.1, 0.3, 0.3)]);
        assert_eq!(pages[2].words, []);
    }

    #[test]
    fn photo_words_and_size_are_upright() {
        let frames = [Frame {
            page_no: None,
            width: 640.0,
            height: 480.0,
        }];
        let mut pages = [ocr_page(1, "Valve")];
        let elements = [ocr_word(1, "Valve", 64, 96, 128, 96)];
        add_words(&mut pages, &elements, &frames, None, Some(picture(6)));
        assert_eq!(pages[0].words, [Word(0, 5, 0.6, 0.1, 0.8, 0.3)]);
        assert_eq!(
            (pages[0].width, pages[0].height),
            (Some(480.0), Some(640.0))
        );
    }

    fn pdf(page: &str, content: &str) -> PdfDocument {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R {page} /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            ),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        PdfDocument::from_bytes(out).unwrap()
    }

    fn layer_words(page: &str, content: &str) -> (Page, Vec<String>) {
        let pdf = pdf(page, content);
        let text = pdf.extract_text(0).unwrap();
        let mut pages = [Page {
            page_no: 1,
            text: text.clone(),
            method: Method::Layer,
            ocr_confidence: None,
            width: None,
            height: None,
            words: Vec::new(),
        }];
        add_words(&mut pages, &[], &[], Some(&pdf), None);
        let chars: Vec<char> = text.chars().collect();
        let [page] = pages;
        let slices = page
            .words
            .iter()
            .map(|w| chars[w.0 as usize..w.1 as usize].iter().collect())
            .collect();
        (page, slices)
    }

    fn word_box(word: &Word) -> [f32; 4] {
        [word.2, word.3, word.4, word.5]
    }

    #[test]
    fn upright_page_keeps_every_word() {
        let (page, slices) = layer_words(
            "/MediaBox [0 0 200 100]",
            "BT /F1 10 Tf 1 0 0 1 20 50 Tm (Alpha beta gamma) Tj ET",
        );
        assert_eq!(slices, ["Alpha", "beta", "gamma"]);
        assert_eq!((page.width, page.height), (Some(200.0), Some(100.0)));
        close(word_box(&page.words[0]), [0.1, 0.4, 0.2279, 0.5]);
    }

    #[test]
    fn turned_text_on_a_turned_page_reads_upright() {
        let upright = layer_words(
            "/MediaBox [0 0 200 100]",
            "BT /F1 10 Tf 1 0 0 1 20 50 Tm (Alpha beta gamma) Tj ET",
        )
        .0;
        for (page, content) in [
            (
                "/MediaBox [0 0 100 200] /Rotate 90",
                "BT /F1 10 Tf 0 1 -1 0 50 20 Tm (Alpha beta gamma) Tj ET",
            ),
            (
                "/MediaBox [0 0 100 200] /Rotate 270",
                "BT /F1 10 Tf 0 -1 1 0 50 180 Tm (Alpha beta gamma) Tj ET",
            ),
            (
                "/MediaBox [10 20 110 220] /Rotate 270",
                "q 0 -1 1 0 0 0 cm BT /F1 10 Tf 1 0 0 1 -200 60 Tm (Alpha beta gamma) Tj ET Q",
            ),
        ] {
            let (turned, slices) = layer_words(page, content);
            assert_eq!(slices, ["Alpha", "beta", "gamma"], "{page}");
            assert_eq!((turned.width, turned.height), (Some(200.0), Some(100.0)));
            for (a, b) in turned.words.iter().zip(&upright.words) {
                close(word_box(a), word_box(b));
            }
        }
    }

    #[test]
    fn upright_text_on_a_turned_page_turns_with_it() {
        let (page, slices) = layer_words(
            "/MediaBox [0 0 100 200] /Rotate 90",
            "BT /F1 10 Tf 1 0 0 1 20 50 Tm (Alpha) Tj ET",
        );
        assert_eq!(slices, ["Alpha"]);
        close(word_box(&page.words[0]), [0.25, 0.2, 0.3, 0.4557]);
    }

    #[test]
    fn untrusted_boxes_are_dropped() {
        let (page, _) = layer_words(
            "/MediaBox [0 0 200 100] /Rotate 180",
            "BT /F1 10 Tf -1 0 0 -1 180 50 Tm (Alpha beta) Tj ET",
        );
        assert_eq!(page.words, []);
        let (page, slices) = layer_words(
            "/MediaBox [0 0 200 100]",
            "BT /F1 10 Tf 1 0 0 1 20 50 Tm (Alpha) Tj 0 1 -1 0 150 10 Tm (axis) Tj ET",
        );
        assert_eq!(slices, ["Alpha"]);
        assert_eq!(page.words.len(), 1);
    }

    #[test]
    fn crop_box_sets_the_page_size() {
        let (page, _) = layer_words(
            "/MediaBox [0 0 200 100] /CropBox [20 0 120 100]",
            "BT /F1 10 Tf 1 0 0 1 20 50 Tm (Alpha) Tj ET",
        );
        assert_eq!((page.width, page.height), (Some(100.0), Some(100.0)));
        close(word_box(&page.words[0]), [0.0, 0.4, 0.2557, 0.5]);
    }

    #[test]
    fn mean_recognition_reads_lines_only() {
        let mut word = element(1, 0.1);
        word.level = OcrElementLevel::Word;
        assert_eq!(
            mean_recognition(&[word, element(1, 0.7)], Some(1)),
            Some(0.7)
        );
    }

    #[test]
    fn cpu_provider_sets_cpu_acceleration() {
        let config = crate::config::load(None).unwrap();
        let settings = ExtractSettings::new(&config.extract, &config.ocr, config.gpu).unwrap();
        assert_eq!(
            settings.xberg.acceleration,
            Some(AccelerationConfig {
                provider: ExecutionProviderType::Cpu,
                device_id: 0,
            })
        );
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn cuda_provider_sets_device() {
        let config = crate::config::load(None).unwrap();
        let gpu = GpuConfig {
            provider: GpuProvider::Cuda,
            device: 1,
        };
        let settings = ExtractSettings::new(&config.extract, &config.ocr, gpu).unwrap();
        assert_eq!(
            settings.xberg.acceleration,
            Some(AccelerationConfig {
                provider: ExecutionProviderType::Cuda,
                device_id: 1,
            })
        );
    }
}
