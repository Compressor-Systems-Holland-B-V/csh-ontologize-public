use std::path::{Path, PathBuf};

use serde_json::Value;
use zontologize::config;
use zontologize::scan::extract::{ExtractSettings, Page, extract_pages};

const SLACK: f32 = 0.015;

fn pages_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pages")
}

fn settings() -> ExtractSettings {
    let config = config::load(None).unwrap();
    ExtractSettings::new(&config.extract, &config.ocr, config.gpu).unwrap()
}

fn plain(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn problems(expected: &Value, pages: &[Page]) -> Vec<String> {
    let expected = expected["pages"].as_array().unwrap();
    if pages.len() != expected.len() {
        return vec![format!(
            "{} pages, expected {}",
            pages.len(),
            expected.len()
        )];
    }
    let mut found = Vec::new();
    for (page, want) in pages.iter().zip(expected) {
        let n = page.page_no;
        let (width, height) = (
            want["width"].as_f64().unwrap() as f32,
            want["height"].as_f64().unwrap() as f32,
        );
        let near = |a: Option<f32>, b: f32| a.is_some_and(|a| (a - b).abs() <= 0.01 * b);
        if !near(page.width, width) || !near(page.height, height) {
            found.push(format!(
                "page {n}: frame {:?}x{:?}, expected {width}x{height}",
                page.width, page.height
            ));
        }
        let chars: Vec<char> = page.text.chars().collect();
        let slice = |a: u32, b: u32| {
            chars
                .get(a as usize..b as usize)
                .map(|s| s.iter().collect::<String>())
        };
        let mut boxes = Vec::new();
        for word in &page.words {
            match slice(word.0, word.1) {
                Some(text) if !text.trim().is_empty() && text.trim() == text => {
                    boxes.push((plain(&text), [word.2, word.3, word.4, word.5]))
                }
                _ => {
                    found.push(format!(
                        "page {n}: word {}-{} does not index the text",
                        word.0, word.1
                    ));
                    break;
                }
            }
        }
        let words = want["words"].as_array().unwrap();
        let matched = words
            .iter()
            .filter(|w| {
                let w = w.as_array().unwrap();
                let text = plain(w[0].as_str().unwrap());
                let b: Vec<f32> = w[1..].iter().map(|v| v.as_f64().unwrap() as f32).collect();
                boxes.iter().any(|(seen, [x0, y0, x1, y1])| {
                    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                    seen.contains(&text)
                        && (b[0] - SLACK..=b[2] + SLACK).contains(&cx)
                        && (b[1] - SLACK..=b[3] + SLACK).contains(&cy)
                })
            })
            .count();
        let least = if want["method"] == "layer" { 1.0 } else { 0.9 };
        if (matched as f32) < least * words.len() as f32 {
            found.push(format!(
                "page {n}: {matched} of {} words in place",
                words.len()
            ));
        }
    }
    found
}

async fn check(name: &str) {
    let path = pages_dir().join(name);
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{}.json", path.display())).unwrap())
            .unwrap();
    let pages = extract_pages(Path::new(&path), &settings()).await.unwrap();
    let read: Vec<_> = pages
        .iter()
        .map(|p| {
            (
                p.method,
                p.width,
                p.height,
                p.text.chars().take(80).collect::<String>(),
                p.words.iter().take(3).collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        problems(&expected, &pages),
        Vec::<String>::new(),
        "{name}: {read:?}"
    );
}

#[tokio::test]
async fn layer_rotated() {
    check("layer-rotated.pdf").await;
}

#[tokio::test]
async fn layer_cropped() {
    check("layer-cropped.pdf").await;
}

#[tokio::test]
#[ignore]
async fn photo_exif() {
    check("photo-exif.jpg").await;
}

#[tokio::test]
#[ignore]
async fn scan_rotated() {
    check("scan-rotated.pdf").await;
}

#[tokio::test]
#[ignore]
async fn scan_sideways() {
    check("scan-sideways.pdf").await;
}

#[tokio::test]
#[ignore]
async fn scan_upside_down() {
    check("scan-upside-down.pdf").await;
}

#[tokio::test]
#[ignore]
async fn scan_drawn_turned() {
    check("scan-drawn-turned.pdf").await;
}

#[tokio::test]
#[ignore]
async fn scan_stamped() {
    check("scan-stamped.pdf").await;
}

#[tokio::test]
#[ignore]
async fn scan_two_dpi() {
    check("scan-two-dpi.pdf").await;
}
