//! Weapon search, browsing and display components.

use dioxus::prelude::*;

use crate::api::{Query, WeaponInfo, looks_like_riven_id};
use crate::ui::{format_age, format_plat};

pub fn group_label(group: &str) -> &str {
    match group {
        "primary" => "Primary",
        "secondary" => "Secondary",
        "melee" => "Melee",
        "archgun" => "Archgun",
        "zaw" => "Zaw",
        "kitgun" => "Kitgun",
        "sentinel" => "Companion",
        "" => "Unknown",
        other => other,
    }
}

const GROUPS: &[&str] = &[
    "primary",
    "secondary",
    "melee",
    "archgun",
    "zaw",
    "kitgun",
    "sentinel",
];

/// Lower is better; `None` means no match.
fn match_rank(w: &WeaponInfo, needle: &str) -> Option<u8> {
    let name = w.name.to_lowercase();
    if name == needle {
        Some(0)
    } else if name.starts_with(needle) {
        Some(1)
    } else if name.split([' ', '-']).any(|word| word.starts_with(needle)) {
        Some(2)
    } else if name.contains(needle) || w.slug.contains(&needle.replace(' ', "_")) {
        Some(3)
    } else {
        None
    }
}

#[component]
pub fn WeaponImage(slug: String, has_image: bool, name: String, size: &'static str) -> Element {
    let mut failed = use_signal(|| false);
    // Reset when the component is reused for another weapon.
    let mut shown_for = use_signal(|| slug.clone());
    if *shown_for.peek() != slug {
        shown_for.set(slug.clone());
        failed.set(false);
    }

    if !has_image || failed() {
        let initial = name.chars().next().unwrap_or('?');
        return rsx! {
            div { class: "weapon-img placeholder {size}", "{initial}" }
        };
    }
    rsx! {
        img {
            class: "weapon-img {size}",
            src: "/img/weapon/{slug}.png",
            alt: "{name}",
            loading: "lazy",
            onerror: move |_| failed.set(true),
        }
    }
}

/// Search box with a dropdown of matching weapon names.
#[component]
pub fn WeaponPicker(weapons: Vec<WeaponInfo>, query: Signal<Query>) -> Element {
    let mut query = query;
    let mut text = use_signal(String::new);
    let mut open = use_signal(|| false);
    let mut active = use_signal(|| 0usize);

    let selected = query
        .read()
        .weapon
        .as_ref()
        .and_then(|slug| weapons.iter().find(|w| &w.slug == slug).cloned());

    let needle = text().trim().to_lowercase();
    let matches: Vec<WeaponInfo> = if needle.is_empty() {
        let mut top: Vec<WeaponInfo> = weapons.iter().filter(|w| w.listings > 0).cloned().collect();
        top.sort_by_key(|w| std::cmp::Reverse(w.median_price));
        top.truncate(12);
        top
    } else {
        let mut ranked: Vec<(u8, &WeaponInfo)> = weapons
            .iter()
            .filter_map(|w| match_rank(w, &needle).map(|r| (r, w)))
            .collect();
        ranked.sort_by(|(ra, a), (rb, b)| {
            ra.cmp(rb)
                .then(b.listings.cmp(&a.listings))
                .then(a.name.cmp(&b.name))
        });
        ranked
            .into_iter()
            .take(12)
            .map(|(_, w)| w.clone())
            .collect()
    };
    let count = matches.len();
    let active_index = active().min(count.saturating_sub(1));

    let id_like = looks_like_riven_id(&needle);
    let mut choose = move |slug: String| {
        query.with_mut(|q| {
            q.weapon = Some(slug);
            q.riven_id = None;
        });
        text.set(String::new());
        open.set(false);
        active.set(0);
    };
    let mut find_id = move |id: String| {
        query.with_mut(|q| q.riven_id = Some(id));
        text.set(String::new());
        open.set(false);
    };
    let picks = matches.clone();

    rsx! {
        div { class: "picker",
            div { class: "picker-box",
                if let Some(w) = selected.as_ref().filter(|_| text().is_empty()) {
                    WeaponImage {
                        slug: w.slug.clone(),
                        has_image: w.has_image,
                        name: w.name.clone(),
                        size: "xs",
                    }
                }
                input {
                    id: "weapon-search",
                    r#type: "text",
                    placeholder: match &selected {
                        Some(w) => w.name.clone(),
                        None => "Search weapons, paste a riven id or screenshot…".to_owned(),
                    },
                    class: if selected.is_some() { "has-selection" } else { "" },
                    value: "{text}",
                    autocomplete: "off",
                    spellcheck: "false",
                    onfocus: move |_| open.set(true),
                    onfocusout: move |_| open.set(false),
                    oninput: move |e| {
                        text.set(e.value());
                        open.set(true);
                        active.set(0);
                    },
                    onkeydown: move |e| match e.key() {
                        Key::ArrowDown => {
                            e.prevent_default();
                            open.set(true);
                            if count > 0 {
                                active.set((active_index + 1) % count);
                            }
                        }
                        Key::ArrowUp => {
                            e.prevent_default();
                            if count > 0 {
                                active.set((active_index + count - 1) % count);
                            }
                        }
                        Key::Enter => {
                            if id_like {
                                find_id(text().trim().to_ascii_lowercase());
                            } else if let Some(w) = picks.get(active_index) {
                                choose(w.slug.clone());
                            }
                        }
                        Key::Escape => {
                            text.set(String::new());
                            open.set(false);
                        }
                        _ => {}
                    },
                }
                if selected.is_some() {
                    button {
                        class: "picker-clear",
                        title: "All weapons",
                        onclick: move |_| {
                            query.with_mut(|q| q.weapon = None);
                            text.set(String::new());
                        },
                        "×"
                    }
                }
                crate::scan::ScanButton {}
            }
            if open() && id_like {
                div { class: "picker-menu",
                    div {
                        class: "picker-item active",
                        onmousedown: move |e: MouseEvent| {
                            e.prevent_default();
                            find_id(text().trim().to_ascii_lowercase());
                        },
                        div { class: "weapon-img placeholder sm", "#" }
                        div { class: "picker-name",
                            div { "Find riven id " code { "{needle}" } }
                            div { class: "muted small", "Enter · every auction of that riven, including gone ones" }
                        }
                    }
                }
            } else if open() && count > 0 {
                div { class: "picker-menu",
                    if needle.is_empty() {
                        div { class: "picker-hint", "Most valuable right now" }
                    }
                    for (i, w) in matches.into_iter().enumerate() {
                        div {
                            key: "{w.slug}",
                            class: if i == active_index { "picker-item active" } else { "picker-item" },
                            onmouseenter: move |_| active.set(i),
                            // mousedown fires before the input loses focus.
                            onmousedown: {
                                let slug = w.slug.clone();
                                move |e: MouseEvent| {
                                    e.prevent_default();
                                    choose(slug.clone());
                                }
                            },
                            WeaponImage {
                                slug: w.slug.clone(),
                                has_image: w.has_image,
                                name: w.name.clone(),
                                size: "sm",
                            }
                            div { class: "picker-name",
                                div { "{w.name}" }
                                div { class: "muted small", "{group_label(&w.group)}" }
                            }
                            div { class: "picker-stats",
                                if let Some(m) = w.median_price {
                                    div { class: "plat-small", "{format_plat(m)}p" }
                                }
                                div { class: "muted small",
                                    if w.listings > 0 {
                                        "{w.listings} listings"
                                    } else {
                                        "not scraped yet"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Header card for the selected weapon.
#[component]
pub fn WeaponHeader(weapon: WeaponInfo, now: i64) -> Element {
    let w = &weapon;
    let url = format!(
        "https://warframe.market/auctions/search?type=riven&weapon_url_name={}&sort_by=price_asc",
        w.slug
    );
    rsx! {
        div { class: "weapon-header",
            WeaponImage {
                slug: w.slug.clone(),
                has_image: w.has_image,
                name: w.name.clone(),
                size: "lg",
            }
            div { class: "weapon-header-title",
                h2 { "{w.name}" }
                div { class: "muted",
                    "{group_label(&w.group)} · {w.class} riven"
                    if w.last_scrape > 0 {
                        " · scraped {format_age(now, w.last_scrape)} ago"
                    }
                }
                a { class: "small", href: "{url}", target: "_blank", rel: "noopener",
                    "warframe.market ↗"
                }
            }
            div { class: "kpis",
                div { class: "kpi",
                    div { class: "kpi-value", "{w.listings}" }
                    div { class: "kpi-label", "listings" }
                }
                div { class: "kpi",
                    div { class: "kpi-value",
                        {w.median_price.map(|p| format!("{}p", format_plat(p))).unwrap_or_else(|| "—".into())}
                    }
                    div { class: "kpi-label", "median" }
                }
                div { class: "kpi",
                    div { class: "kpi-value",
                        {w.min_price.map(|p| format!("{}p", format_plat(p))).unwrap_or_else(|| "—".into())}
                    }
                    div { class: "kpi-label", "cheapest" }
                }
                div { class: "kpi", title: "Listings that disappeared in the last 7 days (sold or delisted), median last price",
                    div { class: "kpi-value",
                        "{w.gone_7d}"
                        if let Some(p) = w.gone_median_7d {
                            span { class: "kpi-sub", " @ {format_plat(p)}p" }
                        }
                    }
                    div { class: "kpi-label", "gone 7d" }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum GridSort {
    Value,
    Listings,
    Name,
}

/// Browsable weapon cards, shown when no weapon is selected.
#[component]
pub fn WeaponGrid(weapons: Vec<WeaponInfo>, query: Signal<Query>) -> Element {
    let mut query = query;
    let mut group = use_signal(|| None::<&'static str>);
    let mut sort = use_signal(|| GridSort::Value);
    let mut show_all = use_signal(|| false);

    let mut list: Vec<WeaponInfo> = weapons
        .iter()
        .filter(|w| group().is_none_or(|g| w.group == g))
        .cloned()
        .collect();
    match sort() {
        GridSort::Value => list.sort_by(|a, b| {
            b.median_price
                .cmp(&a.median_price)
                .then(b.listings.cmp(&a.listings))
        }),
        GridSort::Listings => list.sort_by_key(|w| std::cmp::Reverse(w.listings)),
        GridSort::Name => list.sort_by(|a, b| a.name.cmp(&b.name)),
    }
    let total = list.len();
    if !show_all() {
        list.truncate(24);
    }

    rsx! {
        div { class: "grid-head",
            h2 { "Browse weapons" }
            div { class: "tabs",
                button {
                    class: if group().is_none() { "tab active" } else { "tab" },
                    onclick: move |_| group.set(None),
                    "All"
                }
                for g in GROUPS.iter().copied() {
                    button {
                        key: "{g}",
                        class: if group() == Some(g) { "tab active" } else { "tab" },
                        onclick: move |_| group.set(Some(g)),
                        "{group_label(g)}"
                    }
                }
            }
            select {
                class: "compact",
                onchange: move |e| {
                    sort.set(
                        match e.value().as_str() {
                            "listings" => GridSort::Listings,
                            "name" => GridSort::Name,
                            _ => GridSort::Value,
                        },
                    )
                },
                option { value: "value", "Most valuable" }
                option { value: "listings", "Most listed" }
                option { value: "name", "Name" }
            }
        }
        div { class: "weapon-grid",
            for w in list {
                div {
                    key: "{w.slug}",
                    class: if w.listings == 0 { "weapon-card empty" } else { "weapon-card" },
                    onclick: {
                        let slug = w.slug.clone();
                        move |_| {
                            let slug = slug.clone();
                            query.with_mut(|q| q.weapon = Some(slug));
                        }
                    },
                    WeaponImage {
                        slug: w.slug.clone(),
                        has_image: w.has_image,
                        name: w.name.clone(),
                        size: "md",
                    }
                    div { class: "weapon-card-name", "{w.name}" }
                    div { class: "muted small", "{group_label(&w.group)}" }
                    div { class: "weapon-card-stats",
                        if let Some(m) = w.median_price {
                            span { class: "plat-small", "{format_plat(m)}p" }
                            span { class: "muted small", " median" }
                        } else if w.listings == 0 {
                            span { class: "muted small", "not scraped yet" }
                        } else {
                            span { class: "muted small", "no verified prices" }
                        }
                    }
                    div { class: "muted small",
                        "{w.listings} listings"
                        if w.gone_7d > 0 {
                            " · {w.gone_7d} gone"
                        }
                    }
                }
            }
        }
        if total > 24 {
            button {
                class: "show-all",
                onclick: move |_| show_all.toggle(),
                if show_all() { "Show fewer" } else { "Show all {total} weapons" }
            }
        }
    }
}
