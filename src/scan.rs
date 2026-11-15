//! Scanning a riven screenshot from the search bar.
//!
//! Click the image button, paste a screenshot, or drop one on the page. The OCR runs in the
//! browser (`ocr/`), in a Web Worker that is only created the first time someone hovers or
//! clicks a scan button or pastes or drops an image, so it never slows down loading the site. A
//! scan then opens the scanned riven's page, which shows the screenshot next to what was read.

use dioxus::prelude::*;
use dioxus::router::Navigator;
use serde::Deserialize;

use crate::api::{Overview, RivenScan, ScanAttr, attribute_name};
use crate::ui::Route;
use wfmt_types::riven;

/// What the last scan read, so the page it opens can show what to double-check.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanNote {
    /// The matched riven, or the scanned riven page's parameter.
    pub target: String,
    /// What was read, so a wrong match can still be opened and corrected.
    pub scan: String,
    pub warnings: Vec<String>,
    /// An object URL of the screenshot. It only lives as long as the tab, like this note.
    pub image: Option<String>,
}

pub static LAST_SCAN: GlobalSignal<Option<ScanNote>> = Signal::global(|| None);

/// Shared by the search bar's button and the scanned riven page, so both show a scan in progress
/// however it was started.
static SCAN_STATE: GlobalSignal<State> = Signal::global(|| State::Idle);

/// Shared by every script below. Defining these imports and downloads nothing; the first
/// `scanner()` call does.
const COMMON: &str = r#"
const scanner = () => (window.__wfmtScanner ??= import("/ocr/scanner.js")
  .then((m) => m.createRivenScanner({
    models: "/ocr/models",
    weapons: "/ocr/riven_weapons.json",
    attributes: "/ocr/riven_attributes.json",
  }))
  .catch((error) => { window.__wfmtScanner = undefined; throw error; }));
const scanFile = async (file) => {
  // Kept to show next to the result; the page revokes it once it's replaced.
  const image = URL.createObjectURL(file);
  try {
    dioxus.send({ state: "loading" });
    const s = await scanner();
    await s.preload();
    dioxus.send({ state: "scanning" });
    const { riven } = await s.scan(file);
    dioxus.send({ riven, image });
  } catch (error) {
    URL.revokeObjectURL(image);
    dioxus.send({ error: String(error?.message ?? error) });
  }
};
"#;

/// Starts loading while the pointer is on the button, so it's ready once a file is picked.
const PRELOAD: &str = r#"
scanner().then((s) => s.preload()).catch(() => {});
"#;

/// Asks for an image and scans it. The file dialog opens first, while the click still counts
/// as user activation.
const PICK: &str = r#"
const input = Object.assign(document.createElement("input"), { type: "file", accept: "image/*" });
input.style.display = "none";
document.body.append(input);
const file = await new Promise((resolve) => {
  input.addEventListener("change", () => resolve(input.files[0] ?? null), { once: true });
  input.addEventListener("cancel", () => resolve(null), { once: true });
  input.click();
});
input.remove();
if (file) await scanFile(file);
else dioxus.send({ cancelled: true });
await new Promise(() => {});
"#;

/// Scans images pasted into the search box, or anywhere outside a text field, and image files
/// dropped anywhere on the page. Text pastes and drops are left alone. Replaces the listeners
/// of an earlier search box, so there's only ever one set.
const LISTEN: &str = r#"
for (const [type, listener] of window.__wfmtScanListeners ?? []) document.removeEventListener(type, listener);
let busy = false;
const start = (file) => {
  if (busy) return;
  busy = true;
  scanFile(file).finally(() => { busy = false; });
};
const image = (files) => [...(files ?? [])].find((f) => f.type.startsWith("image/"));
const hasFiles = (event) => [...(event.dataTransfer?.types ?? [])].includes("Files");
// Marks the page while a file is dragged over it, so drop targets can light up.
const dragging = (on) => document.documentElement.classList.toggle("scan-dragging", on);
let depth = 0;
window.__wfmtScanListeners = [
  ["paste", (event) => {
    const target = event.target;
    const field = target instanceof HTMLElement
      && (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName));
    if (field && target.id !== "weapon-search") return;
    const file = image(event.clipboardData?.files);
    if (!file) return;
    event.preventDefault();
    start(file);
  }],
  ["dragenter", (event) => { if (hasFiles(event)) { depth++; dragging(true); } }],
  ["dragleave", (event) => { if (hasFiles(event) && --depth <= 0) { depth = 0; dragging(false); } }],
  // Without this the browser opens a dropped file instead of letting the page have it.
  ["dragover", (event) => { if (hasFiles(event)) event.preventDefault(); }],
  ["drop", (event) => {
    if (!hasFiles(event)) return;
    event.preventDefault();
    depth = 0;
    dragging(false);
    const file = image(event.dataTransfer.files);
    if (file) start(file);
  }],
];
for (const [type, listener] of window.__wfmtScanListeners) document.addEventListener(type, listener);
await new Promise(() => {});
"#;

#[derive(Deserialize)]
struct ScanEvent {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    riven: Option<RivenScan>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    cancelled: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Idle,
    Busy(&'static str),
    Failed(String, Vec<String>),
}

/// Frees a screenshot's object URL once nothing shows it any more.
fn revoke(image: &str) {
    // Object URLs are `blob:<origin>/<uuid>`, so this can't break out of the string.
    if image.starts_with("blob:") && !image.contains(['"', '\\']) {
        document::eval(&format!("URL.revokeObjectURL(\"{image}\");"));
    }
}

/// Follows a scanning script's events. A file pick ends after one scan; the paste and drop
/// listeners keep going.
async fn follow(mut script: document::Eval, navigator: Navigator, once: bool) {
    loop {
        match script.recv::<ScanEvent>().await {
            Ok(ScanEvent {
                cancelled: true, ..
            }) => *SCAN_STATE.write() = State::Idle,
            Ok(ScanEvent {
                error: Some(error), ..
            }) => {
                let message = if error.contains("/ocr/") || error.contains("import") {
                    "Screenshot scanning isn't set up on this server.".to_owned()
                } else {
                    format!("Scanning failed: {error}")
                };
                *SCAN_STATE.write() = State::Failed(message, Vec::new());
            }
            Ok(ScanEvent {
                riven: Some(scan),
                image,
                ..
            }) => open(scan, image, navigator),
            Ok(ScanEvent { state: Some(s), .. }) => {
                *SCAN_STATE.write() = State::Busy(match s.as_str() {
                    "loading" => "Loading the scanner…",
                    _ => "Reading the riven…",
                });
                continue;
            }
            Ok(_) => continue,
            Err(error) => {
                if once {
                    *SCAN_STATE.write() =
                        State::Failed(format!("Scanning failed: {error}"), Vec::new());
                }
                return;
            }
        }
        if once {
            return;
        }
    }
}

/// Always open the editable reading, including partial scans and scans matched to a listing.
fn open(scan: RivenScan, image: Option<String>, navigator: Navigator) {
    if scan.weapon.is_none() && scan.attributes.is_empty() {
        if let Some(image) = &image {
            revoke(image);
        }
        *SCAN_STATE.write() = State::Failed(
            "Couldn't find a riven in that image. Use a screenshot of one riven card.".into(),
            scan.warnings,
        );
        return;
    }
    let param = scan.to_param();
    let previous = LAST_SCAN.write().replace(ScanNote {
        target: param.clone(),
        scan: param.clone(),
        warnings: scan.warnings,
        image: image.clone(),
    });
    if let Some(old) = previous
        .and_then(|n| n.image)
        .filter(|old| Some(old) != image.as_ref())
    {
        revoke(&old);
    }
    *SCAN_STATE.write() = State::Idle;
    navigator.push(Route::ScannedRivenPage { scan: param });
}

/// Opens the file dialog and scans the picked image.
fn pick(navigator: Navigator) {
    if matches!(*SCAN_STATE.read(), State::Busy(_)) {
        return;
    }
    spawn(async move {
        follow(document::eval(&format!("{COMMON}{PICK}")), navigator, true).await;
    });
}

/// Starts downloading the scanner, for when a scan is likely to follow.
fn preload() {
    document::eval(&format!("{COMMON}{PRELOAD}"));
}

/// An image icon.
#[component]
fn ImageIcon(size: u32) -> Element {
    rsx! {
        svg { view_box: "0 0 24 24", width: "{size}", height: "{size}", fill: "none", stroke: "currentColor",
            stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
            rect { x: "3", y: "3", width: "18", height: "18", rx: "2" }
            circle { cx: "8.5", cy: "8.5", r: "1.5" }
            path { d: "M21 15l-5-5L5 21" }
        }
    }
}

/// The image button in the search bar, which also scans pasted and dropped screenshots.
#[component]
pub fn ScanButton() -> Element {
    let navigator = use_navigator();
    let busy = matches!(SCAN_STATE(), State::Busy(_));

    // Effects only run in the browser. Installing the listeners downloads nothing.
    use_effect(move || {
        spawn(async move {
            follow(
                document::eval(&format!("{COMMON}{LISTEN}")),
                navigator,
                false,
            )
            .await;
        });
    });
    use_drop(|| {
        document::eval(
            r#"
            for (const [type, listener] of window.__wfmtScanListeners ?? []) {
              document.removeEventListener(type, listener);
            }
            delete window.__wfmtScanListeners;
            document.documentElement.classList.remove("scan-dragging");
            "#,
        );
    });

    rsx! {
        button {
            class: if busy { "picker-scan busy" } else { "picker-scan" },
            r#type: "button",
            title: "Scan a riven screenshot (or paste or drop one)",
            aria_label: "Scan a riven screenshot",
            onpointerenter: move |_| preload(),
            onfocus: move |_| preload(),
            onclick: move |_| pick(navigator),
            ImageIcon { size: 18 }
        }
        match SCAN_STATE() {
            State::Idle => rsx! {},
            State::Busy(message) => rsx! {
                div { class: "scan-status", role: "status", span { class: "spinner" } "{message}" }
            },
            State::Failed(message, warnings) => rsx! {
                div { class: "scan-status error", role: "alert",
                    div { class: "scan-status-head",
                        span { "{message}" }
                        button { class: "picker-clear", title: "Dismiss", onclick: move |_| *SCAN_STATE.write() = State::Idle, "×" }
                    }
                    if !warnings.is_empty() {
                        ul { class: "muted small", for w in warnings { li { "{w}" } } }
                    }
                }
            },
        }
    }
}

/// The screenshot a riven was scanned from, beside what was read so the two can be compared.
/// Without one (a shared link, or after a reload), a place to scan it.
#[component]
pub fn ScanScreenshot(target: String) -> Element {
    let navigator = use_navigator();
    let image = LAST_SCAN
        .read()
        .as_ref()
        .filter(|n| n.target == target)
        .and_then(|n| n.image.clone());
    let busy = match SCAN_STATE() {
        State::Busy(message) => Some(message),
        _ => None,
    };
    let scan_button = rsx! {
        button {
            class: "scan-shot-button",
            r#type: "button",
            disabled: busy.is_some(),
            onpointerenter: move |_| preload(),
            onfocus: move |_| preload(),
            onclick: move |_| pick(navigator),
            ImageIcon { size: 16 }
            if image.is_some() { "Scan another" } else { "Scan a screenshot" }
        }
    };
    rsx! {
        aside { class: "scan-shot", aria_label: "Your screenshot",
            div { class: if image.is_some() { "scan-shot-frame" } else { "scan-shot-frame empty" },
                if let Some(src) = &image {
                    a { href: "{src}", target: "_blank", rel: "noopener", title: "Open the full-size screenshot",
                        img { src: "{src}", alt: "The riven screenshot you scanned" }
                    }
                } else {
                    div { class: "scan-shot-empty",
                        ImageIcon { size: 32 }
                        p { "Scan a screenshot of your riven to see it here, next to what was read." }
                    }
                }
                if let Some(message) = busy {
                    div { class: "scan-shot-busy", role: "status", span { class: "spinner" } "{message}" }
                }
                div { class: "scan-shot-drop", "Drop to scan" }
            }
            div { class: "scan-shot-foot",
                span { class: "muted small",
                    if image.is_some() { "Your screenshot" } else { "Or paste or drop one anywhere" }
                }
                {scan_button}
            }
        }
    }
}

/// What to double-check, shown on the page a scan opened.
#[component]
pub fn ScanNotice(target: String, matched: bool) -> Element {
    let note = LAST_SCAN.read().clone().filter(|n| n.target == target);
    // The scanned riven's own page says where its stats came from; only warnings need saying.
    if note
        .as_ref()
        .is_none_or(|n| !matched && n.warnings.is_empty())
    {
        return rsx! {};
    }
    let scan = note.as_ref().map(|n| n.scan.clone()).unwrap_or_default();
    let warnings = note.map(|n| n.warnings).unwrap_or_default();
    rsx! {
        div { class: if matched { "scan-note" } else { "scan-note warn" },
            if matched {
                "Matched your screenshot to this riven, which has been listed on warframe.market before. "
                Link { to: Route::ScannedRivenPage { scan }, "Wrong riven? Edit what the scan read" }
            } else {
                strong { "Double-check these against your screenshot:" }
            }
            if !warnings.is_empty() {
                ul { for w in warnings { li { "{w}" } } }
            }
        }
    }
}

/// Whether the stats add up to a possible riven (see `wfmt_types::riven`).
pub fn valid_layout(attributes: &[ScanAttr]) -> bool {
    let positives = attributes.iter().filter(|a| a.positive).count();
    riven::valid_layout(positives, attributes.len() - positives)
}

/// What follows a stat's number on the riven card.
fn unit(slug: &str) -> &'static str {
    match slug {
        s if s.starts_with("damage_vs_") || s.starts_with("damage_to_") => "×",
        "punch_through" | "range" => "m",
        "combo_duration" => "s",
        "channeling_damage" => "",
        _ => "%",
    }
}

/// What the scanner read, editable, since OCR can misread a digit or a stat. Every change is
/// passed up whole; selects apply at once, text and numbers when the field is left.
#[component]
pub fn ScanEditor(scan: RivenScan, onchange: EventHandler<RivenScan>) -> Element {
    let loaded = use_context::<Signal<Option<Overview>>>();
    let Some(overview) = loaded() else {
        return rsx! {};
    };
    let mut weapons: Vec<(String, String)> = overview
        .weapons
        .iter()
        .map(|w| (w.slug.clone(), w.name.clone()))
        .collect();
    weapons.sort_by(|a, b| a.1.cmp(&b.1));
    let class = scan
        .weapon
        .as_ref()
        .and_then(|slug| overview.weapons.iter().find(|w| &w.slug == slug))
        .map(|w| w.class);
    // Stats this weapon can roll, plus any the scan has that it can't, so nothing vanishes.
    let mut stats: Vec<(String, String, bool)> = overview
        .attributes
        .iter()
        .map(|a| {
            let fits = class.as_ref().is_none_or(|c| a.classes.contains(c));
            (a.slug.clone(), a.name.clone(), fits)
        })
        .filter(|(slug, _, fits)| *fits || scan.attributes.iter().any(|a| &a.slug == slug))
        .collect();
    // An unrecognized stat must remain visible until the user corrects it.
    for attr in &scan.attributes {
        if !stats.iter().any(|(slug, _, _)| slug == &attr.slug) {
            stats.push((attr.slug.clone(), attribute_name(&attr.slug), false));
        }
    }
    stats.sort_by(|a, b| a.1.cmp(&b.1));
    let number = |n: Option<i64>| n.map(|n| n.to_string()).unwrap_or_default();

    // Each edit starts from the scan as it is now.
    let current = scan.clone();
    let edit = move |change: Box<dyn FnOnce(&mut RivenScan)>| {
        let mut next = current.clone();
        change(&mut next);
        onchange.call(next);
    };
    let edit = std::rc::Rc::new(edit);

    rsx! {
        section { class: "scan-editor",
            div { class: "scan-editor-head",
                h3 { "What the scanner read" }
                span { class: "muted small", "Fix anything that doesn't match; the grade below updates as you go." }
            }
            div { class: "scan-fields",
                label {
                    span { "Weapon" }
                    select {
                        value: "{scan.weapon.clone().unwrap_or_default()}",
                        onchange: {
                            let edit = edit.clone();
                            move |e: FormEvent| {
                                let v = e.value();
                                edit(Box::new(move |s| s.weapon = (!v.is_empty()).then_some(v)));
                            }
                        },
                        if scan.weapon.is_none() { option { value: "", "Choose a weapon" } }
                        if let Some(slug) = scan.weapon.as_ref().filter(|s| !weapons.iter().any(|(w, _)| w == *s)) {
                            option { value: "{slug}", "{slug} (unknown weapon)" }
                        }
                        for (slug, name) in weapons {
                            option { key: "{slug}", value: "{slug}", "{name}" }
                        }
                    }
                }
                label {
                    span { "Riven name" }
                    input {
                        r#type: "text",
                        value: "{scan.name.clone().unwrap_or_default()}",
                        placeholder: "e.g. Argi-cronicon",
                        onchange: {
                            let edit = edit.clone();
                            move |e: FormEvent| {
                                let v = e.value().trim().to_lowercase();
                                edit(Box::new(move |s| s.name = (!v.is_empty()).then_some(v)));
                            }
                        },
                    }
                }
                label {
                    span { "Mastery rank" }
                    input {
                        r#type: "number", min: "0", max: "16", inputmode: "numeric",
                        value: "{number(scan.mastery_rank)}",
                        onchange: {
                            let edit = edit.clone();
                            move |e: FormEvent| {
                                let v = e.value().trim().parse().ok();
                                edit(Box::new(move |s| s.mastery_rank = v));
                            }
                        },
                    }
                }
                label {
                    span { "Rerolls" }
                    input {
                        r#type: "number", min: "0", inputmode: "numeric",
                        value: "{number(scan.rerolls)}",
                        onchange: {
                            let edit = edit.clone();
                            move |e: FormEvent| {
                                let v = e.value().trim().parse().ok();
                                edit(Box::new(move |s| s.rerolls = v));
                            }
                        },
                    }
                }
            }

            div { class: "scan-stats",
                div { class: "scan-stats-head",
                    span { class: "scan-label", "Stats" }
                    span { class: "muted small", "In card order. Click + or − to flip a stat between buff and curse." }
                }
                if scan.attributes.is_empty() {
                    p { class: "muted small", "No stats were read. Add them from your screenshot." }
                }
                for (i, a) in scan.attributes.iter().enumerate() {
                    div { key: "{i}-{a.slug}", class: if a.positive { "scan-stat positive" } else { "scan-stat negative" },
                        button {
                            class: "scan-sign",
                            r#type: "button",
                            aria_pressed: a.positive,
                            aria_label: if a.positive { "Positive stat, make it negative" } else { "Negative stat, make it positive" },
                            title: if a.positive { "Buff. Click to make it a curse." } else { "Curse. Click to make it a buff." },
                            onclick: {
                                let edit = edit.clone();
                                let positive = !a.positive;
                                move |_| edit(Box::new(move |s| s.attributes[i].positive = positive))
                            },
                            if a.positive { "+" } else { "−" }
                        }
                        select {
                            class: "scan-stat-name",
                            aria_label: "Stat",
                            value: "{a.slug}",
                            onchange: {
                                let edit = edit.clone();
                                move |e: FormEvent| {
                                    let slug = e.value();
                                    edit(Box::new(move |s| s.attributes[i].slug = slug));
                                }
                            },
                            for (slug, name, fits) in stats.iter() {
                                option { key: "{slug}", value: "{slug}",
                                    match (*fits, crate::api::is_spliced(slug)) {
                                        (false, _) => format!("{name} (not on this weapon)"),
                                        (true, true) => format!("{name} (spliced)"),
                                        (true, false) => name.clone(),
                                    }
                                }
                            }
                        }
                        div { class: "scan-stat-value",
                            input {
                                r#type: "number", step: "any", inputmode: "decimal",
                                aria_label: "{attribute_name(&a.slug)} value",
                                value: "{a.value}",
                                onchange: {
                                    let edit = edit.clone();
                                    move |e: FormEvent| {
                                        if let Ok(v) = e.value().trim().replace(',', ".").parse::<f64>() {
                                            if !v.is_finite() { return; }
                                            edit(Box::new(move |s| s.attributes[i].value = v));
                                        }
                                    }
                                },
                            }
                            span { class: "muted", "{unit(&a.slug)}" }
                        }
                        button {
                            class: "picker-clear scan-stat-remove",
                            r#type: "button",
                            title: "Remove this stat",
                            aria_label: "Remove {attribute_name(&a.slug)}",
                            onclick: {
                                let edit = edit.clone();
                                move |_| edit(Box::new(move |s| { s.attributes.remove(i); }))
                            },
                            "×"
                        }
                    }
                }
                if scan.attributes.len() < riven::MAX_STATS {
                    button {
                        class: "scan-add",
                        r#type: "button",
                        onclick: {
                            let edit = edit.clone();
                            let unused = stats
                                .iter()
                                .find(|(slug, _, fits)| *fits && !scan.attributes.iter().any(|a| &a.slug == slug))
                                .map(|(slug, _, _)| slug.clone());
                            move |_| {
                                if let Some(slug) = unused.clone() {
                                    edit(Box::new(move |s| {
                                        let positive = s.attributes.iter().filter(|a| a.positive).count() < *riven::POSITIVES.end();
                                        s.attributes.push(ScanAttr { slug, value: 0.0, positive });
                                    }));
                                }
                            }
                        },
                        "+ Add a stat"
                        span { class: "muted", " ({scan.attributes.len()} of {riven::MAX_STATS})" }
                    }
                }
                if !valid_layout(&scan.attributes) {
                    p { class: "scan-note warn",
                        "A riven has 2 or 3 positive stats and at most one negative, so this can't be graded yet."
                    }
                }
            }
        }
    }
}
