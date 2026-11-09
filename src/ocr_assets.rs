//! `/ocr/...`: the screenshot scanner's files (`ocr/scripts/build-dist.sh`), loaded by the
//! browser only when someone scans a riven.
//!
//! They're served from `WFMT_OCR_DIR`, else `ocr/` next to the server binary (the Docker
//! image), else `ocr/dist` in the working directory (development).

use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use dioxus::fullstack::axum::extract::Path;
use dioxus::fullstack::axum::http::{HeaderMap, StatusCode, header};
use dioxus::fullstack::axum::response::{IntoResponse, Response};

static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    if let Some(dir) = std::env::var_os("WFMT_OCR_DIR") {
        return dir.into();
    }
    let beside_binary = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ocr")));
    match beside_binary {
        Some(dir) if dir.is_dir() => dir,
        _ => PathBuf::from("ocr/dist"),
    }
});

fn content_type(path: &FsPath) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        Some("yml") => "text/yaml; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Only plain relative paths inside the directory.
fn resolve(path: &str) -> Option<PathBuf> {
    let relative = FsPath::new(path);
    relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then(|| DIR.join(relative))
}

pub async fn serve(Path(path): Path<String>, headers: HeaderMap) -> Response {
    if path == "riven_weapons.json" {
        return weapons().await;
    }
    let Some(file) = resolve(&path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let accepts_gzip = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"));
    let gzipped = file.with_file_name(format!(
        "{}.gz",
        file.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
    ));
    let (served, encoded) = if accepts_gzip && tokio::fs::metadata(&gzipped).await.is_ok() {
        (gzipped, true)
    } else {
        (file.clone(), false)
    };
    let Ok(meta) = tokio::fs::metadata(&served).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // The names don't change between builds, so browsers revalidate and get a 304 if unchanged.
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let etag = format!(
        "\"{:x}-{modified:x}{}\"",
        meta.len(),
        if encoded { "-gz" } else { "" }
    );
    let common = [
        (header::CONTENT_TYPE, content_type(&file).to_owned()),
        (header::CACHE_CONTROL, "no-cache".to_owned()),
        (header::ETAG, etag.clone()),
        (header::VARY, "Accept-Encoding".to_owned()),
    ];
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag)
    {
        return (StatusCode::NOT_MODIFIED, common).into_response();
    }
    let Ok(bytes) = tokio::fs::read(&served).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if encoded {
        (common, [(header::CONTENT_ENCODING, "gzip")], bytes).into_response()
    } else {
        (common, bytes).into_response()
    }
}

/// The scraper's current weapon list, so new weapons can be scanned, plus the names in other
/// languages from the snapshot bundled with the scanner (the scraper only asks for English).
async fn weapons() -> Response {
    let snapshot = tokio::fs::read(DIR.join("riven_weapons.json")).await.ok();
    let current = tokio::fs::read(crate::data::data_dir().join("cache/riven_weapons.json"))
        .await
        .ok();
    let body = tokio::task::spawn_blocking(move || merge_weapons(current, snapshot))
        .await
        .ok()
        .flatten();
    match body {
        Some(body) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn merge_weapons(current: Option<Vec<u8>>, snapshot: Option<Vec<u8>>) -> Option<Vec<u8>> {
    use serde_json::Value;
    let parse = |bytes: &Option<Vec<u8>>| -> Option<Value> {
        serde_json::from_slice(bytes.as_deref()?).ok()
    };
    let (Some(mut current), snapshot) = (parse(&current), parse(&snapshot)) else {
        return snapshot;
    };
    if let (Some(weapons), Some(Value::Array(extra))) = (
        current["data"].as_array_mut(),
        snapshot.map(|s| s["data"].clone()),
    ) {
        for weapon in weapons {
            let Some(names) = extra
                .iter()
                .find(|e| e["slug"] == weapon["slug"])
                .and_then(|e| e["i18n"].as_object())
            else {
                continue;
            };
            if let Some(i18n) = weapon["i18n"].as_object_mut() {
                for (language, name) in names {
                    i18n.entry(language.clone()).or_insert_with(|| name.clone());
                }
            }
        }
    }
    serde_json::to_vec(&current).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_directory() {
        assert!(resolve("pkg/wfmt_ocr_bg.wasm").is_some());
        assert!(resolve("models/tiny_det/inference.onnx").is_some());
        assert!(resolve("../Cargo.toml").is_none());
        assert!(resolve("/etc/passwd").is_none());
        assert!(resolve("pkg/../../x").is_none());
    }

    #[test]
    fn weapons_keep_current_list_and_gain_other_languages() {
        let current = br#"{"data":[{"slug":"soma","i18n":{"en":{"name":"Soma"}}},{"slug":"new_gun","i18n":{"en":{"name":"New Gun"}}}]}"#;
        let snapshot =
            r#"{"data":[{"slug":"soma","i18n":{"en":{"name":"Old"},"zh-hans":{"name":"月神"}}}]}"#
                .as_bytes();
        let merged: serde_json::Value = serde_json::from_slice(
            &merge_weapons(Some(current.to_vec()), Some(snapshot.to_vec())).unwrap(),
        )
        .unwrap();
        assert_eq!(merged["data"][0]["i18n"]["en"]["name"], "Soma");
        assert_eq!(merged["data"][0]["i18n"]["zh-hans"]["name"], "月神");
        assert_eq!(merged["data"][1]["slug"], "new_gun");
        assert!(merge_weapons(None, Some(snapshot.to_vec())).is_some());
    }
}
