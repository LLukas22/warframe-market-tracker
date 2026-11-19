//! Browser-owned rules and notifications stay alive across route changes in the shell.
use crate::api::Overview;
use dioxus::prelude::*;

#[component]
pub fn AlertRuntime() -> Element {
    rsx! { div { hidden: true, onmounted: move |_| { document::eval(include_str!("../assets/alerts.js")); } } }
}

#[component]
pub fn AlertsPage() -> Element {
    let overview = use_context::<Signal<Option<Overview>>>();
    let catalog = overview.read();
    rsx! {
        main { class: "alerts-page", id: "riven-alerts",
            div { class: "alerts-intro",
                h1 { "Riven alerts" }
                div { class: "alert-notifications",
                    p { id: "alerts-permission-status", role: "status" }
                    button { id: "alerts-permission", r#type: "button", disabled: true, "Enable notifications" }
                }
            }
            form { id: "alert-form", class: "alert-form", aria_label: "Create an alert",
                label { r#for: "alert-weapon", "Weapon"
                    input {
                        id: "alert-weapon", r#type: "text", list: "alert-weapons", required: true,
                        disabled: catalog.is_none(), autocomplete: "off", spellcheck: false,
                        placeholder: if catalog.is_none() { "Loading weapons…" } else { "Search by weapon name" },
                    }
                }
                datalist { id: "alert-weapons",
                    if let Some(data) = catalog.as_ref() {
                        for weapon in &data.weapons {
                            option { key: "{weapon.slug}", value: "{weapon.name}", "data-weapon": "{weapon.slug}" }
                        }
                    }
                }
                label { r#for: "alert-price", "Max price (platinum)"
                    input { id: "alert-price", r#type: "number", min: "1", step: "1", max: "2147483647",
                        required: true, placeholder: "e.g. 300" }
                }
                button { class: "alert-submit", r#type: "submit", disabled: true, "Save alert" }
                details { class: "alert-seller-filters",
                    summary { "Seller filters" span { class: "alert-filter-summary", id: "alert-filter-summary", "Online or in game" } }
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
            }
            p { id: "alerts-error", class: "error", role: "alert" }
            p { id: "alerts-saved", class: "alert-saved", role: "status" }
            section { class: "alerts-section", aria_labelledby: "alert-rules-title",
                h2 { id: "alert-rules-title", "Watchlist" }
                div { id: "alerts-rules",
                    p { class: "alert-empty", "Loading saved alerts…" }
                }
            }
            section { class: "alerts-section", aria_labelledby: "alert-matches-title",
                h2 { id: "alert-matches-title", "Recent matches" }
                p { id: "alerts-status", role: "status", class: "alert-check-status", hidden: true }
                div { id: "alerts-matches", aria_live: "polite" }
            }
        }
    }
}
