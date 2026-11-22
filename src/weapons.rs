//! Weapon search, browsing and display components.

use dioxus::prelude::*;

use wfmt_types::{WeaponClass, WeaponGroup};

use crate::api::{WeaponInfo, looks_like_riven_id};
use crate::ui::{Route, format_age, format_plat};

pub fn group_label(group: WeaponGroup) -> &'static str {
    match group {
        WeaponGroup::Primary => "Primary",
        WeaponGroup::Secondary => "Secondary",
        WeaponGroup::Melee => "Melee",
        WeaponGroup::Archgun => "Archgun",
        WeaponGroup::Zaw => "Zaw",
        WeaponGroup::Kitgun => "Kitgun",
        WeaponGroup::Sentinel => "Companion",
        WeaponGroup::Unknown => "Unknown",
    }
}

/// The weapon list's tabs, in order.
const GROUPS: &[WeaponGroup] = &[
    WeaponGroup::Primary,
    WeaponGroup::Secondary,
    WeaponGroup::Melee,
    WeaponGroup::Archgun,
    WeaponGroup::Zaw,
    WeaponGroup::Kitgun,
    WeaponGroup::Sentinel,
];

/// Suggestions the weapon picker lists.
const PICKER_RESULTS: usize = 12;
/// Weapons the start page's grid shows before "show all".
const GRID_COLLAPSED: usize = 24;

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
pub fn WeaponPicker(weapons: Vec<WeaponInfo>) -> Element {
    let selected = match use_route::<Route>() {
        Route::Weapon { slug } => Some(slug),
        _ => None,
    };
    rsx! { WeaponDropdown { weapons, selected } }
}

/// The header navigates to the chosen weapon. The alert form keeps the choice locally.
#[component]
pub fn WeaponDropdown(
    weapons: Vec<WeaponInfo>,
    selected: Option<String>,
    #[props(optional)] on_change: Option<EventHandler<Option<String>>>,
) -> Element {
    let navigator = use_navigator();
    let cache = use_context::<crate::ui::ViewCache>();
    let filters = use_context::<Signal<crate::api::Query>>();
    let mut text = use_signal(String::new);
    let mut open = use_signal(|| false);
    let mut active = use_signal(|| 0usize);

    let market = on_change.is_none();
    let input_id = if market {
        "weapon-search"
    } else {
        "alert-weapon"
    };
    let selected = selected
        .as_ref()
        .and_then(|slug| weapons.iter().find(|w| &w.slug == slug));
    let has_selection = selected.is_some();

    let needle = text().trim().to_lowercase();
    let matches: Vec<WeaponInfo> = if needle.is_empty() {
        let mut top: Vec<WeaponInfo> = weapons.iter().filter(|w| w.listings > 0).cloned().collect();
        top.sort_by_key(|w| std::cmp::Reverse(w.median_price));
        top.truncate(PICKER_RESULTS);
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
            .take(PICKER_RESULTS)
            .map(|(_, w)| w.clone())
            .collect()
    };
    let count = matches.len();
    let active_index = active().min(count.saturating_sub(1));

    let id_like = market && looks_like_riven_id(&needle);
    let close = move || {
        let (mut text, mut open, mut active) = (text, open, active);
        text.set(String::new());
        open.set(false);
        active.set(0);
    };
    let choose = move |slug: String| {
        if let Some(on_change) = on_change {
            on_change.call(Some(slug));
        } else {
            navigator.push(Route::Weapon { slug });
        }
        close();
    };
    let mut find_id = move |id: String| {
        navigator.push(Route::Search { id });
        text.set(String::new());
        open.set(false);
    };
    let picks = matches.clone();

    rsx! {
        div { class: "picker",
            div { class: "picker-box",
                if let Some(w) = selected.filter(|_| text().is_empty()) {
                    WeaponImage {
                        slug: w.slug.clone(),
                        has_image: w.has_image,
                        name: w.name.clone(),
                        size: "xs",
                    }
                }
                input {
                    id: input_id,
                    r#type: "text",
                    aria_label: if market { "Search weapons or riven ID" } else { "Weapon" },
                    disabled: !market && weapons.is_empty(),
                    placeholder: match selected {
                        Some(w) => w.name.clone(),
                        None if !market && weapons.is_empty() => "Loading weapons…".into(),
                        None if !market => "Search by weapon name".into(),
                        None => "Search weapons, paste a riven id or screenshot…".to_owned(),
                    },
                    class: if has_selection { "has-selection" } else { "" },
                    value: if !market && let Some(w) = selected { w.name.clone() } else { text() },
                    role: (!market).then_some("combobox"),
                    aria_autocomplete: (!market).then_some("list"),
                    aria_expanded: (!market).then(|| open() && count > 0),
                    aria_controls: (!market && open() && count > 0).then_some("alert-weapon-options"),
                    aria_activedescendant: (!market && open() && count > 0).then(|| format!("alert-weapon-option-{active_index}")),
                    autocomplete: "off",
                    spellcheck: "false",
                    onfocus: move |_| open.set(true),
                    onfocusout: move |_| open.set(false),
                    oninput: move |e| {
                        if let Some(on_change) = on_change.filter(|_| has_selection) {
                            on_change.call(None);
                        }
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
                        Key::Enter if market || open() => {
                            e.prevent_default();
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
                if has_selection && market {
                    Link {
                        to: Route::Market {},
                        class: "picker-clear",
                        title: "All weapons",
                        onclick: move |_| {
                            text.set(String::new());
                            open.set(false);
                        },
                        "×"
                    }
                } else if has_selection && let Some(on_change) = on_change {
                    button {
                        class: "picker-clear", r#type: "button", aria_label: "Clear weapon",
                        onclick: move |_| { on_change.call(None); close(); },
                        "×"
                    }
                }
                if market {
                    crate::scan::ScanButton {}
                }
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
                    id: (!market).then_some("alert-weapon-options"),
                    role: (!market).then_some("listbox"),
                    aria_label: (!market).then_some("Weapons"),
                    if needle.is_empty() {
                        div { class: "picker-hint", aria_hidden: !market, "Most valuable right now" }
                    }
                    for (i, w) in matches.into_iter().enumerate() {
                        div {
                            key: "{w.slug}",
                            onmouseenter: {
                                let slug = w.slug.clone();
                                move |_| {
                                    active.set(i);
                                    if market {
                                        cache.prefetch_weapon(filters.peek().clone(), slug.clone());
                                    }
                                }
                            },
                            // Keep the menu mounted until the link receives its click.
                            onmousedown: move |e: MouseEvent| e.prevent_default(),
                            if market {
                                Link {
                                    to: Route::Weapon { slug: w.slug.clone() },
                                    class: if i == active_index { "picker-item active" } else { "picker-item" },
                                    onclick: move |_| close(),
                                    WeaponSuggestion { weapon: w.clone(), show_price: true }
                                }
                            } else {
                                button {
                                    id: "alert-weapon-option-{i}",
                                    role: "option",
                                    aria_selected: i == active_index,
                                    r#type: "button", tabindex: -1,
                                    class: if i == active_index { "picker-item active" } else { "picker-item" },
                                    onclick: {
                                        let slug = w.slug.clone();
                                        move |_| choose(slug.clone())
                                    },
                                    WeaponSuggestion { weapon: w.clone(), show_price: false }
                                }
                            }
                        }
                    }
                }
            } else if !market && open() && !needle.is_empty() {
                div { class: "picker-menu", role: "status",
                    div { class: "picker-empty", "No weapons match" }
                }
            }
        }
    }
}

#[component]
fn WeaponSuggestion(weapon: WeaponInfo, show_price: bool) -> Element {
    rsx! {
        WeaponImage {
            slug: weapon.slug.clone(),
            has_image: weapon.has_image,
            name: weapon.name.clone(),
            size: "sm",
        }
        div { class: "picker-name",
            div { "{weapon.name}" }
            div { class: "muted small", "{group_label(weapon.group)}" }
        }
        div { class: "picker-stats",
            if show_price && let Some(m) = weapon.median_price {
                div { class: "plat-small", "{format_plat(m)}p" }
            }
            div { class: "muted small",
                if weapon.listings > 0 { "{weapon.listings} listings" } else { "not scraped yet" }
            }
        }
    }
}

/// Header card for the selected weapon.
#[component]
pub fn WeaponHeader(weapon: WeaponInfo, now: Option<i64>) -> Element {
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
                    "{group_label(w.group)} · {w.class.map_or(\"unknown\", WeaponClass::as_str)} riven"
                    if let Some(now) = now.filter(|_| w.last_scrape > 0) {
                        " · scraped {format_age(now, w.last_scrape)} ago"
                    }
                }
                div { class: "weapon-header-links small",
                    a { href: "{url}", target: "_blank", rel: "noopener", "warframe.market ↗" }
                    Link { to: Route::AlertsPage { weapon: Some(w.slug.clone()) }, "Create alert" }
                }
            }
            div { class: "kpis",
                div { class: "kpi", title: "Every current listing of this weapon, before any filter",
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
                div { class: "kpi", title: "Listings that disappeared in the last 7 days (sold or delisted), and their median last price",
                    div { class: "kpi-value", "{w.gone_7d}" }
                    div { class: "kpi-label",
                        "gone 7d"
                        if let Some(p) = w.gone_median_7d { " · {format_plat(p)}p" }
                    }
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

impl GridSort {
    const ALL: [Self; 3] = [Self::Value, Self::Listings, Self::Name];

    /// How the `<select>` names it.
    fn key(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Listings => "listings",
            Self::Name => "name",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Value => "Most valuable",
            Self::Listings => "Most listed",
            Self::Name => "Name",
        }
    }
}

/// The grid's tab, sort and expansion. Owned by the app shell so they survive visiting a
/// weapon and coming back.
#[derive(Clone, Copy)]
pub struct GridState {
    group: Signal<Option<WeaponGroup>>,
    sort: Signal<GridSort>,
    show_all: Signal<bool>,
}

impl GridState {
    pub fn new() -> Self {
        Self {
            group: Signal::new(None),
            sort: Signal::new(GridSort::Value),
            show_all: Signal::new(false),
        }
    }
}

/// Browsable weapon cards, shown when no weapon is selected.
#[component]
pub fn WeaponGrid(weapons: Vec<WeaponInfo>) -> Element {
    let GridState {
        mut group,
        mut sort,
        mut show_all,
    } = use_context();
    let cache = use_context::<crate::ui::ViewCache>();
    let filters = use_context::<Signal<crate::api::Query>>();
    // Hovering a card starts loading its page, so the click usually finds it ready.
    let prefetch = move |slug: String| cache.prefetch_weapon(filters.peek().clone(), slug);

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
        list.truncate(GRID_COLLAPSED);
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
                        key: "{g.as_str()}",
                        class: if group() == Some(g) { "tab active" } else { "tab" },
                        onclick: move |_| group.set(Some(g)),
                        "{group_label(g)}"
                    }
                }
            }
            select {
                class: "compact",
                onchange: move |e| {
                    let key = e.value();
                    sort.set(GridSort::ALL.into_iter().find(|s| s.key() == key).unwrap_or(GridSort::Value));
                },
                for option_sort in GridSort::ALL {
                    option {
                        key: "{option_sort.key()}",
                        value: option_sort.key(),
                        selected: sort() == option_sort,
                        "{option_sort.label()}"
                    }
                }
            }
        }
        div { class: "weapon-grid",
            for w in list {
                // A cell around the link, which takes no event handlers besides clicks.
                div {
                    key: "{w.slug}",
                    class: "weapon-cell",
                    onmouseenter: {
                        let slug = w.slug.clone();
                        move |_| prefetch(slug.clone())
                    },
                    Link {
                        to: Route::Weapon { slug: w.slug.clone() },
                        class: if w.listings == 0 { "weapon-card empty" } else { "weapon-card" },
                        WeaponImage {
                            slug: w.slug.clone(),
                            has_image: w.has_image,
                            name: w.name.clone(),
                            size: "md",
                        }
                        div { class: "weapon-card-name", "{w.name}" }
                        div { class: "muted small", "{group_label(w.group)}" }
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
        }
        if total > GRID_COLLAPSED {
            button {
                class: "show-all",
                onclick: move |_| show_all.toggle(),
                if show_all() { "Show fewer" } else { "Show all {total} weapons" }
            }
        }
    }
}
