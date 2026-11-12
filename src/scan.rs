//! Scanning a riven screenshot from the search bar.
//!
//! Click the image button or paste a screenshot. The OCR runs in the browser (`ocr/`), in a
//! Web Worker that is only created the first time someone hovers or clicks the scan button or
//! pastes an image, so it never slows down loading the site. A scan
//! then opens the riven's page: the stored riven if it has been listed before, else a page for
//! the scanned riven.

use dioxus::prelude::*;
use dioxus::router::Navigator;
use serde::Deserialize;

use crate::api::{Overview, RivenScan, ScanAttr, attribute_name};
use crate::ui::Route;

/// What the last scan read, so the page it opens can show what to double-check.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanNote {
    /// The matched riven, or the scanned riven page's parameter.
    pub target: String,
    /// What was read, so a wrong match can still be opened and corrected.
    pub scan: String,
    pub warnings: Vec<String>,
}

pub static LAST_SCAN: GlobalSignal<Option<ScanNote>> = Signal::global(|| None);

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
  try {
    dioxus.send({ state: "loading" });
    const s = await scanner();
    await s.preload();
    dioxus.send({ state: "scanning" });
    const { riven } = await s.scan(file);
    dioxus.send({ riven });
  } catch (error) {
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

/// Scans images pasted into the search box, or anywhere outside a text field. Text pastes are
/// left alone. Replaces the listener of an earlier search box, so there's only ever one.
const PASTE: &str = r#"
if (window.__wfmtPaste) document.removeEventListener("paste", window.__wfmtPaste);
let busy = false;
window.__wfmtPaste = (event) => {
  const target = event.target;
  const field = target instanceof HTMLElement
    && (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName));
  if (field && target.id !== "weapon-search") return;
  const file = [...(event.clipboardData?.files ?? [])].find((f) => f.type.startsWith("image/"));
  if (!file) return;
  event.preventDefault();
  if (busy) return;
  busy = true;
  scanFile(file).finally(() => { busy = false; });
};
document.addEventListener("paste", window.__wfmtPaste);
await new Promise(() => {});
"#;

#[derive(Deserialize)]
struct ScanEvent {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    riven: Option<RivenScan>,
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

/// Follows a scanning script's events. A file pick ends after one scan; the paste listener
/// keeps going.
async fn follow(
    mut script: document::Eval,
    mut state: Signal<State>,
    navigator: Navigator,
    once: bool,
) {
    loop {
        match script.recv::<ScanEvent>().await {
            Ok(ScanEvent {
                cancelled: true, ..
            }) => state.set(State::Idle),
            Ok(ScanEvent {
                error: Some(error), ..
            }) => {
                let message = if error.contains("/ocr/") || error.contains("import") {
                    "Screenshot scanning isn't set up on this server.".to_owned()
                } else {
                    format!("Scanning failed: {error}")
                };
                state.set(State::Failed(message, Vec::new()));
            }
            Ok(ScanEvent {
                riven: Some(scan), ..
            }) => open(scan, state, navigator).await,
            Ok(ScanEvent { state: Some(s), .. }) => {
                state.set(State::Busy(match s.as_str() {
                    "loading" => "Loading the scanner…",
                    _ => "Reading the riven…",
                }));
                continue;
            }
            Ok(_) => continue,
            Err(error) => {
                if once {
                    state.set(State::Failed(
                        format!("Scanning failed: {error}"),
                        Vec::new(),
                    ));
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
async fn open(scan: RivenScan, mut state: Signal<State>, navigator: Navigator) {
    if scan.weapon.is_none() && scan.attributes.is_empty() {
        return state.set(State::Failed(
            "Couldn't find a riven in that image. Use a screenshot of one riven card.".into(),
            scan.warnings,
        ));
    }
    let target = Route::ScannedRivenPage {
        scan: scan.to_param(),
    };
    *LAST_SCAN.write() = Some(ScanNote {
        target: match &target {
            Route::RivenPage { id, .. } => id.clone(),
            Route::ScannedRivenPage { scan } => scan.clone(),
            _ => String::new(),
        },
        scan: scan.to_param(),
        warnings: scan.warnings,
    });
    state.set(State::Idle);
    navigator.push(target);
}

/// The image button in the search bar, which also scans pasted screenshots.
#[component]
pub fn ScanButton() -> Element {
    let mut state = use_signal(|| State::Idle);
    let navigator = use_navigator();
    let busy = matches!(state(), State::Busy(_));

    // Effects only run in the browser. Installing the listener downloads nothing.
    use_effect(move || {
        spawn(async move {
            follow(
                document::eval(&format!("{COMMON}{PASTE}")),
                state,
                navigator,
                false,
            )
            .await;
        });
    });
    use_drop(|| {
        document::eval(
            r#"
            if (window.__wfmtPaste) {
              document.removeEventListener("paste", window.__wfmtPaste);
              delete window.__wfmtPaste;
            }
            "#,
        );
    });

    let preload = || {
        document::eval(&format!("{COMMON}{PRELOAD}"));
    };
    let start = move |_| {
        if busy {
            return;
        }
        spawn(async move {
            follow(
                document::eval(&format!("{COMMON}{PICK}")),
                state,
                navigator,
                true,
            )
            .await;
        });
    };

    rsx! {
        button {
            class: if busy { "picker-scan busy" } else { "picker-scan" },
            r#type: "button",
            title: "Scan a riven screenshot (or paste one)",
            aria_label: "Scan a riven screenshot",
            onpointerenter: move |_| preload(),
            onfocus: move |_| preload(),
            onclick: start,
            // An image icon.
            svg { view_box: "0 0 24 24", width: "18", height: "18", fill: "none", stroke: "currentColor",
                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
                rect { x: "3", y: "3", width: "18", height: "18", rx: "2" }
                circle { cx: "8.5", cy: "8.5", r: "1.5" }
                path { d: "M21 15l-5-5L5 21" }
            }
        }
        match state() {
            State::Idle => rsx! {},
            State::Busy(message) => rsx! {
                div { class: "scan-status", role: "status", span { class: "spinner" } "{message}" }
            },
            State::Failed(message, warnings) => rsx! {
                div { class: "scan-status error", role: "alert",
                    div { class: "scan-status-head",
                        span { "{message}" }
                        button { class: "picker-clear", title: "Dismiss", onclick: move |_| state.set(State::Idle), "×" }
                    }
                    if !warnings.is_empty() {
                        ul { class: "muted small", for w in warnings { li { "{w}" } } }
                    }
                }
            },
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
        div { class: "scan-note",
            if matched {
                "Matched your screenshot to this riven, which has been listed on warframe.market before. "
                Link { to: Route::ScannedRivenPage { scan }, "Wrong riven? Edit what the scan read" }
            } else {
                "The scanner wasn't sure about some of this, so check it against your riven:"
            }
            if !warnings.is_empty() {
                ul { for w in warnings { li { "{w}" } } }
            }
        }
    }
}

/// Positive and negative stats a riven can have: 2 or 3 positives, at most one negative.
pub fn valid_layout(attributes: &[ScanAttr]) -> bool {
    let positives = attributes.iter().filter(|a| a.positive).count();
    (2..=3).contains(&positives) && attributes.len() - positives <= 1
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
        .map(|w| w.class.clone());
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
                h3 { "Read from your screenshot" }
                span { class: "muted small", "Fix anything the scanner got wrong; the page updates as you go." }
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
                div { class: "scan-stats-head muted small",
                    span { "Stats, as shown on the card" }
                }
                for (i, a) in scan.attributes.iter().enumerate() {
                    div { key: "{i}-{a.slug}", class: if a.positive { "scan-stat positive" } else { "scan-stat negative" },
                        select {
                            class: "scan-stat-kind",
                            aria_label: "Positive or negative",
                            value: if a.positive { "p" } else { "n" },
                            onchange: {
                                let edit = edit.clone();
                                move |e: FormEvent| {
                                    let positive = e.value() == "p";
                                    edit(Box::new(move |s| s.attributes[i].positive = positive));
                                }
                            },
                            option { value: "p", "+ positive" }
                            option { value: "n", "− negative" }
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
                                    if *fits { "{name}" } else { "{name} (not on this weapon)" }
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
                            class: "picker-clear",
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
                if scan.attributes.len() < 4 {
                    button {
                        class: "link-button",
                        onclick: {
                            let edit = edit.clone();
                            let unused = stats
                                .iter()
                                .find(|(slug, _, fits)| *fits && !scan.attributes.iter().any(|a| &a.slug == slug))
                                .map(|(slug, _, _)| slug.clone());
                            move |_| {
                                if let Some(slug) = unused.clone() {
                                    edit(Box::new(move |s| {
                                        let positive = s.attributes.iter().filter(|a| a.positive).count() < 3;
                                        s.attributes.push(ScanAttr { slug, value: 0.0, positive });
                                    }));
                                }
                            }
                        },
                        "+ Add a stat"
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
