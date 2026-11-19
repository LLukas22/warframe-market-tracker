//! Browser-owned rules and notifications stay alive across route changes in the shell.
use crate::api::Overview;
use crate::weapons::WeaponDropdown;
use dioxus::prelude::*;

#[component]
pub fn AlertRuntime() -> Element {
    rsx! { div { hidden: true, onmounted: move |_| { document::eval(include_str!("../assets/alerts.js")); } } }
}

#[component]
pub fn AlertsPage(weapon: Option<String>) -> Element {
    let key = weapon.as_deref().unwrap_or_default();
    rsx! { AlertsView { key: "{key}", weapon } }
}

#[component]
fn AlertsView(weapon: Option<String>) -> Element {
    let mut selected = use_signal(|| weapon);
    let overview = use_context::<Signal<Option<Overview>>>();
    let catalog = overview.read();
    let weapons = catalog
        .as_ref()
        .map(|data| data.weapons.as_slice())
        .unwrap_or_default();
    let choice = selected.read();
    let selected_weapon = choice
        .as_ref()
        .and_then(|slug| weapons.iter().find(|w| &w.slug == slug));
    rsx! {
        main { class: "alerts-page", id: "riven-alerts",
            div { class: "alerts-intro",
                h1 { "Riven alerts" }
                p { class: "muted", "Get pinged when a Riven for your weapon is listed at or under your price." }
            }
            div { class: "alert-notice", id: "alerts-notice", "data-state": "loading",
                p { id: "alerts-permission-status", role: "status" }
                button { id: "alerts-permission", r#type: "button", disabled: true, "Enable notifications" }
            }
            form { id: "alert-form", class: "alert-card alert-form", aria_label: "Create an alert",
                "data-weapon": selected_weapon.map(|w| w.slug.clone()),
                "data-weapon-name": selected_weapon.map(|w| w.name.clone()),
                div { class: "alert-weapon-field",
                    label { r#for: "alert-weapon", "Weapon" }
                    WeaponDropdown {
                        weapons: weapons.to_vec(),
                        selected: choice.clone(),
                        on_change: move |slug| selected.set(slug),
                    }
                }
                label { r#for: "alert-price", "Max price (platinum)"
                    input { id: "alert-price", r#type: "number", min: "1", step: "1", max: "2147483647",
                        required: true, placeholder: "e.g. 300" }
                }
                button { class: "alert-submit", r#type: "submit", disabled: selected_weapon.is_none(), "Save alert" }
                details { class: "alert-seller-filters",
                    summary {
                        "Seller filters"
                        span { class: "alert-filter-summary", id: "alert-filter-summary", "Online or in game" }
                        span { class: "alert-chevron", aria_hidden: true }
                    }
                    div { class: "alert-seller-fields",
                        label { r#for: "alert-seller-status", "Seller availability"
                            select { id: "alert-seller-status",
                                option { value: "available", "Online or in game" }
                                option { value: "ingame", "In game only" }
                                option { value: "online", "Online only" }
                                option { value: "any", "Any status" }
                            }
                        }
                        label { r#for: "alert-reputation", "Minimum reputation"
                            input { id: "alert-reputation", r#type: "number", min: "0", max: "2147483647", step: "1", value: "0", required: true }
                        }
                    }
                    p { "Seller availability reflects the last scrape." }
                }
                p { id: "alerts-error", class: "error", role: "alert" }
                p { id: "alerts-saved", class: "alert-saved", role: "status" }
            }
            section { class: "alert-card alerts-section", aria_labelledby: "alert-rules-title",
                div { class: "section-head",
                    h2 { id: "alert-rules-title", "Watchlist" }
                    span { class: "muted small", id: "alerts-rules-count" }
                }
                div { id: "alerts-rules",
                    p { class: "alert-empty", "Loading saved alerts…" }
                }
            }
            section { class: "alert-card alerts-section", aria_labelledby: "alert-matches-title",
                div { class: "section-head",
                    h2 { id: "alert-matches-title", "Recent matches" }
                    span { id: "alerts-status", role: "status", class: "muted small", hidden: true }
                }
                div { id: "alerts-matches", aria_live: "polite" }
            }
        }
    }
}
