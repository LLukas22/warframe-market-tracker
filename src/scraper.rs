use dioxus::prelude::*;
use wfmt_types::{FetchLog, Phase, ScraperStatus, SweepSummary};

use crate::api::{self, ScraperSnapshot};
use crate::ui::{format_datetime, sleep_ms};

const POLL_MS: u32 = 2000;
/// Give up waiting for a sent command to show in the status after this long.
const PENDING_TIMEOUT_MS: i64 = 30_000;
const NEEDS_TOKEN: &str = "Enter the admin token first";

#[component]
pub fn ScraperPanel() -> Element {
    let mut resource = use_resource(api::scraper_status);
    let mut snapshot = use_signal(|| None::<ScraperSnapshot>);
    let mut connection_error = use_signal(|| None::<String>);
    let mut pending = use_signal(|| None::<(bool, i64)>);
    let mut control_error = use_signal(|| None::<String>);
    let mut token = use_signal(String::new);

    use_effect(move || match &*resource.read() {
        Some(Ok(value)) => {
            snapshot.set(Some(value.clone()));
            connection_error.set(None);
        }
        Some(Err(error)) => connection_error.set(Some(error.to_string())),
        None => {}
    });
    use_effect(move || {
        if let (Some((cancel, requested_at)), Some(value)) = (pending(), snapshot()) {
            let sweeping = value.status.as_ref().is_some_and(|s| s.sweep.is_some());
            if sweeping != cancel
                || !value.running
                || value.server_time - requested_at > PENDING_TIMEOUT_MS
            {
                pending.set(None);
            }
        }
    });
    use_future(move || async move {
        loop {
            sleep_ms(POLL_MS).await;
            resource.restart();
        }
    });

    let control = move |cancel: bool| {
        let requested_at = snapshot.peek().as_ref().map_or(0, |s| s.server_time);
        pending.set(Some((cancel, requested_at)));
        control_error.set(None);
        spawn(async move {
            if let Err(error) = api::scraper_control(cancel, token.peek().clone()).await {
                pending.set(None);
                control_error.set(Some(match error {
                    ServerFnError::ServerError { message, .. } => message,
                    other => other.to_string(),
                }));
            }
            resource.restart();
        });
    };

    let admin = snapshot().is_some_and(|s| s.admin);

    rsx! {
        document::Title { "Scraper · Riven Tracker" }
        main { class: "scraper-page",
            div { class: "scraper-heading",
                h2 { "Scraper" }
                if admin {
                    input {
                        id: "admin-token",
                        r#type: "password",
                        placeholder: "Admin token",
                        aria_label: "Admin token",
                        autocomplete: "current-password",
                        value: "{token}",
                        oninput: move |event| token.set(event.value()),
                    }
                }
            }
            if let Some(value) = snapshot() {
                ScraperCard {
                    snapshot: value,
                    can_control: admin && !token().is_empty(),
                    connected: connection_error().is_none(),
                    pending: pending().map(|p| p.0),
                    oncontrol: control,
                }
            } else {
                div { class: "scraper-card muted",
                    if connection_error().is_some() { "Can't reach the server, retrying…" } else { "Loading…" }
                }
            }
            if let Some(error) = control_error() {
                p { class: "error small", role: "alert", "{error}" }
            }
            if admin {
                DataTransfer { token: token() }
            } else if snapshot().is_some() {
                p { class: "muted small",
                    "Set an admin token on the server to start or cancel scrapes and to export or upload data."
                }
            }
            if let Some(entries) = snapshot().and_then(|s| s.status).map(|s| s.recent).filter(|r| !r.is_empty()) {
                RecentFetches { entries, now: snapshot().map_or(0, |s| s.server_time) }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
enum Upload {
    Idle,
    Sending(u8),
    Done(String),
    Failed(String),
}

#[derive(serde::Deserialize)]
struct UploadEvent {
    percent: Option<u8>,
    done: Option<String>,
    failed: Option<String>,
    #[serde(default)]
    cancelled: bool,
}

/// Runs in the browser, which sends the file straight from disk however large it is.
const UPLOAD_SCRIPT: &str = r#"
const input = document.getElementById('transfer-file');
const file = input.files[0];
const token = document.getElementById('admin-token').value;
input.value = '';
if (!file) {
    dioxus.send({ cancelled: true });
} else if (!confirm(`Replace all data on this server with ${file.name}?`)) {
    dioxus.send({ cancelled: true });
} else {
    const request = new XMLHttpRequest();
    request.open('POST', '/api/data/import');
    request.setRequestHeader('Authorization', `Bearer ${token}`);
    request.upload.onprogress = (e) => {
        if (e.lengthComputable) dioxus.send({ percent: Math.floor(e.loaded * 100 / e.total) });
    };
    request.onload = () => dioxus.send(
        request.status === 200 ? { done: request.responseText } : { failed: request.responseText }
    );
    request.onerror = () => dioxus.send({ failed: 'The upload was interrupted.' });
    request.send(file);
}
await new Promise(() => {});
"#;

#[component]
fn DataTransfer(token: String) -> Element {
    let mut upload = use_signal(|| Upload::Idle);
    let sending = matches!(upload(), Upload::Sending(_));
    let no_token = token.is_empty();

    let start_upload = move |_| {
        upload.set(Upload::Sending(0));
        spawn(async move {
            let mut script = document::eval(UPLOAD_SCRIPT);
            loop {
                let event = match script.recv::<UploadEvent>().await {
                    Ok(event) => event,
                    Err(error) => {
                        upload.set(Upload::Failed(error.to_string()));
                        return;
                    }
                };
                if let Some(percent) = event.percent {
                    upload.set(Upload::Sending(percent));
                } else if let Some(message) = event.done {
                    upload.set(Upload::Done(message));
                    return;
                } else if let Some(message) = event.failed {
                    upload.set(Upload::Failed(message));
                    return;
                } else if event.cancelled {
                    upload.set(Upload::Idle);
                    return;
                }
            }
        });
    };

    rsx! {
        section { class: "scraper-card",
            div { class: "scraper-now",
                strong { "Data" }
                // A plain form, so the browser handles the download. A wrong token only shows in
                // the new tab; a good one closes it again and starts saving the file.
                form { class: "transfer-form", method: "post", action: "/api/data/export", target: "_blank",
                    input { r#type: "hidden", name: "token", value: "{token}" }
                    button {
                        class: "scraper-action",
                        r#type: "submit",
                        disabled: no_token,
                        title: if no_token { NEEDS_TOKEN },
                        "Export"
                    }
                    button {
                        class: "scraper-action",
                        r#type: "button",
                        disabled: no_token || sending,
                        title: if no_token { NEEDS_TOKEN },
                        onclick: move |_| { document::eval("document.getElementById('transfer-file').click()"); },
                        "Upload…"
                    }
                    input {
                        id: "transfer-file",
                        class: "transfer-file",
                        r#type: "file",
                        accept: ".gz,.tgz",
                        tabindex: -1,
                        aria_hidden: true,
                        onchange: start_upload,
                    }
                }
            }
            match upload() {
                Upload::Sending(100) => rsx! { p { class: "muted small", role: "status", "Checking and importing…" } },
                Upload::Sending(percent) => rsx! {
                    progress { value: percent, max: 100, aria_label: "Upload progress" }
                },
                Upload::Done(message) => rsx! { p { class: "muted small", role: "status", "{message}" } },
                Upload::Failed(message) => rsx! { p { class: "error small", role: "alert", "{message}" } },
                Upload::Idle => rsx! {
                    p { class: "muted small",
                        "Export pauses the scraper for a moment. Uploading an export replaces all data here, and the current data is kept in a backup folder."
                    }
                },
            }
        }
    }
}

/// What the scraper is doing, as a short sentence.
fn headline(status: Option<&ScraperStatus>, online: bool, connected: bool) -> String {
    if !connected {
        return "Reconnecting…".into();
    }
    let Some(status) = status.filter(|_| online) else {
        return "Not running".into();
    };
    match status.phase {
        Phase::Metadata => "Updating the weapon list".into(),
        Phase::Maintenance => "Maintaining the archive".into(),
        Phase::Cooldown => "Waiting after a failed fetch".into(),
        Phase::Scraping => status
            .sweep
            .as_ref()
            .and_then(|s| s.current.as_ref())
            .map_or("Scraping".into(), |c| {
                format!("Scraping {}", weapon_name(&c.weapon))
            }),
        Phase::Idle | Phase::Stopped => "Idle".into(),
    }
}

fn next_scrape(status: Option<&ScraperStatus>, now: i64) -> Option<String> {
    let at = status?.next_sweep_at?;
    Some(if at > now {
        format!("in {}", duration(at - now))
    } else if status?.sweep.is_some() {
        "after this one".into()
    } else {
        "now".into()
    })
}

fn last_scrape(last: &SweepSummary, now: i64) -> String {
    let mut text = format!(
        "{} ago · {} auctions",
        duration(now - last.finished_at),
        count(u64::from(last.auctions))
    );
    if last.cancelled {
        text.push_str(" · cancelled");
    }
    if last.failed > 0 {
        text.push_str(&format!(" · {} failed", last.failed));
    }
    text
}

#[component]
fn ScraperCard(
    snapshot: ScraperSnapshot,
    can_control: bool,
    connected: bool,
    pending: Option<bool>,
    oncontrol: EventHandler<bool>,
) -> Element {
    let status = snapshot.status.as_ref();
    let now = snapshot.server_time;
    let online = connected && snapshot.running && status.is_none_or(|s| s.phase != Phase::Stopped);
    let phase = status.map_or(Phase::Idle, |s| s.phase);
    let sweep = status.and_then(|s| s.sweep.as_ref());
    let busy = online && (sweep.is_some() || phase != Phase::Idle);
    // Cancel while a sweep exists, otherwise start one. Nothing can be done during maintenance.
    let cancels = sweep.is_some();
    let enabled = can_control
        && online
        && status.is_some()
        && pending.is_none()
        && phase != Phase::Maintenance
        && (cancels || phase == Phase::Idle);
    let label = match (pending, cancels) {
        (Some(true), _) => "Cancelling…",
        (Some(false), _) => "Starting…",
        (None, true) => "Cancel",
        (None, false) => "Scrape now",
    };

    rsx! {
        section { class: "scraper-card",
            div { class: "scraper-now",
                span { class: if busy { "scraper-dot busy" } else if online { "scraper-dot online" } else { "scraper-dot" } }
                strong { "{headline(status, online, connected)}" }
                button {
                    class: if cancels { "scraper-action" } else { "scraper-action primary" },
                    disabled: !enabled,
                    title: if !can_control { NEEDS_TOKEN },
                    onclick: move |_| oncontrol.call(cancels),
                    "{label}"
                }
            }
            if let Some(s) = sweep {
                progress { value: s.done.min(s.total), max: s.total.max(1), aria_label: "Sweep progress" }
                div { class: "muted small",
                    "{s.done} / {s.total} weapons · {count(u64::from(s.auctions))} auctions"
                    if s.failed > 0 { " · {s.failed} failed" }
                    if !online { " · continues when the scraper starts again" }
                }
            }
            dl {
                div {
                    dt { "Next scrape" }
                    dd {
                        title: status.and_then(|s| s.next_sweep_at).map_or(String::new(), |t| format!("{} UTC", format_datetime(t))),
                        {if online { next_scrape(status, now).unwrap_or_else(|| "—".into()) } else { "—".into() }}
                    }
                }
                div {
                    dt { "Last scrape" }
                    dd {
                        title: status.and_then(|s| s.last_sweep.as_ref()).map_or(String::new(), |l| format!("{} UTC", format_datetime(l.finished_at))),
                        {status.and_then(|s| s.last_sweep.as_ref()).map_or("—".into(), |l| last_scrape(l, now))}
                    }
                }
            }
            if let Some(error) = status.filter(|_| online).and_then(|s| s.error.as_ref()) {
                p { class: "error small", "{error}" }
            }
        }
    }
}

/// Closed by default so the page stays calm; failures are counted on the label.
#[component]
fn RecentFetches(entries: Vec<FetchLog>, now: i64) -> Element {
    let failed = entries.iter().filter(|e| e.error.is_some()).count();
    rsx! {
        details { class: "scraper-recent",
            summary {
                "Recent fetches"
                if failed > 0 { span { class: "error", " · {failed} failed" } }
            }
            ul {
                for entry in entries.iter().take(20) {
                    li { key: "{entry.t}-{entry.weapon}",
                        span { "{weapon_name(&entry.weapon)}" }
                        if entry.error.is_some() {
                            span { class: "error", "failed" }
                        } else {
                            span { class: "muted", "{count(u64::from(entry.auctions))} auctions" }
                        }
                        span { class: "muted", title: "{format_datetime(entry.t)} UTC", "{duration(now - entry.t)} ago" }
                        if let Some(error) = &entry.error {
                            p { class: "muted small", "{error}" }
                        }
                    }
                }
            }
        }
    }
}

fn weapon_name(slug: &str) -> String {
    slug.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn duration(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86400, seconds % 86400 / 3600),
    }
}

/// `1234567` -> `1,234,567`.
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use wfmt_types::{CurrentFetch, Settings, SweepProgress, Trigger};

    fn sample() -> ScraperSnapshot {
        ScraperSnapshot {
            server_time: 1_800_000_000_000,
            running: true,
            admin: true,
            status: Some(ScraperStatus {
                pid: 42,
                started_at: 1_799_999_000_000,
                updated_at: 1_800_000_000_000,
                phase: Phase::Scraping,
                sweep: Some(SweepProgress {
                    started_at: 1_799_999_000_000,
                    trigger: Trigger::Schedule,
                    total: 100,
                    done: 40,
                    failed: 1,
                    auctions: 4200,
                    gone: 12,
                    current: Some(CurrentFetch {
                        weapon: "rubico_prime".into(),
                        started_at: 1_799_999_999_000,
                        step: "price_asc page".into(),
                    }),
                }),
                next_sweep_at: Some(1_800_010_000_000),
                last_sweep: None,
                recent: vec![FetchLog {
                    t: 1_799_999_999_000,
                    weapon: "kulstar".into(),
                    duration_ms: 3400,
                    pages: 2,
                    auctions: 100,
                    gone: 3,
                    error: None,
                }],
                requests: 75,
                settings: Settings {
                    sweep_interval_hours: 12.0,
                    request_interval_secs: 3.0,
                    sort_by: vec!["price_asc".into(), "price_desc".into()],
                    verify_missing: 0,
                    weapons: 100,
                },
                error: None,
            }),
        }
    }

    fn render(snapshot: ScraperSnapshot, connected: bool, pending: Option<bool>) -> String {
        render_with(snapshot, true, connected, pending)
    }

    fn render_with(
        snapshot: ScraperSnapshot,
        can_control: bool,
        connected: bool,
        pending: Option<bool>,
    ) -> String {
        #[derive(Clone, PartialEq, Props)]
        struct TestProps {
            snapshot: ScraperSnapshot,
            can_control: bool,
            connected: bool,
            pending: Option<bool>,
        }
        fn test_card(props: TestProps) -> Element {
            rsx! { ScraperCard { snapshot: props.snapshot, can_control: props.can_control, connected: props.connected, pending: props.pending, oncontrol: |_| {} } }
        }
        let mut dom = VirtualDom::new_with_props(
            test_card,
            TestProps {
                snapshot,
                can_control,
                connected,
                pending,
            },
        );
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    fn button_disabled(html: &str) -> bool {
        let button = html.split("<button").nth(1).expect("action button exists");
        button.split('>').next().unwrap().contains("disabled")
    }

    #[test]
    fn running_sweep_shows_weapon_progress_next_scrape_and_cancel() {
        let html = render(sample(), true, None);
        assert!(html.contains("Scraping Rubico Prime"));
        assert!(html.contains("40 / 100 weapons"));
        assert!(html.contains("4,200 auctions"));
        assert!(html.contains("1 failed"));
        assert!(html.contains("in 2h 46m"));
        assert!(html.contains("Cancel"));
        assert!(!button_disabled(&html));
    }

    #[test]
    fn offline_scraper_never_looks_active() {
        let mut snapshot = sample();
        snapshot.running = false;
        let html = render(snapshot, true, None);
        assert!(html.contains("Not running"));
        assert!(!html.contains("Rubico Prime"));
        assert!(html.contains("continues when the scraper starts again"));
        assert!(button_disabled(&html));

        let disconnected = render(sample(), false, None);
        assert!(disconnected.contains("Reconnecting"));
        assert!(button_disabled(&disconnected));
    }

    #[test]
    fn idle_scraper_offers_scrape_now_until_pending_or_maintenance() {
        let mut snapshot = sample();
        let status = snapshot.status.as_mut().unwrap();
        status.sweep = None;
        status.phase = Phase::Idle;
        let idle = render(snapshot.clone(), true, None);
        assert!(idle.contains("Idle"));
        assert!(idle.contains("Scrape now"));
        assert!(!button_disabled(&idle));

        let pending = render(snapshot.clone(), true, Some(false));
        assert!(pending.contains("Starting…"));
        assert!(button_disabled(&pending));

        snapshot.status.as_mut().unwrap().phase = Phase::Maintenance;
        let maintenance = render(snapshot, true, None);
        assert!(maintenance.contains("Maintaining the archive"));
        assert!(button_disabled(&maintenance));
    }

    #[test]
    fn scrape_controls_need_the_admin_token() {
        let running = render_with(sample(), false, true, None);
        assert!(running.contains("Cancel"));
        assert!(button_disabled(&running));

        let mut idle = sample();
        let status = idle.status.as_mut().unwrap();
        status.sweep = None;
        status.phase = Phase::Idle;
        let idle = render_with(idle, false, true, None);
        assert!(idle.contains("Scrape now"));
        assert!(button_disabled(&idle));
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(999), "999");
        assert_eq!(count(1_234_567), "1,234,567");
    }
}
