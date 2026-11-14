//! Names and messages shared by the crates and the web UI: warframe.market's vocabulary, the
//! riven attributes, and the scraper's [`ScraperStatus`] and [`Command`]s, which the server
//! passes between its engine and the scraper page. All times are unix ms.

use serde::{Deserialize, Serialize};

/// Where the project lives, and how warframe.market can find out who's behind our requests.
#[macro_export]
macro_rules! repository {
    () => {
        "https://github.com/LLukas22/warframe-market-tracker"
    };
}
pub const REPOSITORY: &str = repository!();

/// Sent with every request to warframe.market, whose rules ask each client to identify itself
/// with a name, version and a link: <https://docs.warframe.market/docs/rules/overview/>.
pub const USER_AGENT: &str = concat!(
    "warframe-market-tracker/",
    env!("CARGO_PKG_VERSION"),
    " (+",
    repository!(),
    ")"
);

/// Held by the server whose scraper runs on a data directory, relative to it: one per directory.
pub const LOCK_FILE: &str = "scraper/lock";

pub const SECOND_MS: i64 = 1000;
pub const MINUTE_MS: i64 = 60 * SECOND_MS;
pub const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const DAY_MS: i64 = 24 * HOUR_MS;
pub const WEEK_MS: i64 = 7 * DAY_MS;

/// The engine republishes its status at least this often, so the times it shows stay current.
pub const HEARTBEAT_MS: i64 = 15 * SECOND_MS;

/// The rules every riven follows, for checking scans and edits against.
pub mod riven {
    use std::ops::RangeInclusive;

    /// How many positive stats a riven has.
    pub const POSITIVES: RangeInclusive<usize> = 2..=3;
    /// A riven has at most one negative stat (its curse).
    pub const MAX_NEGATIVES: usize = 1;
    pub const MAX_STATS: usize = *POSITIVES.end() + MAX_NEGATIVES;
    /// A roll scales a stat's base value by a factor in this range.
    pub const ROLL_MIN: f64 = 0.9;
    pub const ROLL_MAX: f64 = 1.1;

    /// Whether this many positive and negative stats make a possible riven.
    pub fn valid_layout(positives: usize, negatives: usize) -> bool {
        POSITIVES.contains(&positives) && negatives <= MAX_NEGATIVES
    }
}

/// Implements `as_str`, `Display` and `FromStr` from a list of variants and their names.
macro_rules! names {
    ($ty:ident { $($variant:ident => $name:literal),* $(,)? }) => {
        impl $ty {
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),*
                }
            }
        }

        impl ::std::fmt::Display for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $ty {
            type Err = $crate::market::UnknownName;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| $crate::market::UnknownName {
                        kind: stringify!($ty),
                        name: s.to_owned(),
                    })
            }
        }
    };
}

pub mod attributes;

pub use market::{Feed, RivenType, SortBy, Unit, WeaponClass, WeaponGroup};

/// Names warframe.market uses, as enums. They serialize to warframe.market's own spelling, which
/// is also how archives and parquet files store them.
pub mod market {
    use std::fmt;

    use serde::{Deserialize, Serialize};

    /// A name none of an enum's variants has.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct UnknownName {
        pub kind: &'static str,
        pub name: String,
    }

    impl fmt::Display for UnknownName {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "unknown {}: {:?}", self.kind, self.name)
        }
    }

    impl std::error::Error for UnknownName {}

    /// warframe.market's site-wide listings. Their ids name the raw archive, the fetch log
    /// entry and the `source` of the rows they produce, where weapon searches put a weapon.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub enum Feed {
        /// The most recent listings.
        #[serde(rename = "feed_recent")]
        Recent,
        /// The popular listings.
        #[serde(rename = "feed_popular")]
        Popular,
    }

    names!(Feed {
        Recent => "feed_recent",
        Popular => "feed_popular",
    });

    impl Feed {
        /// The feed an archive name, log entry or row source stands for, if it's one.
        pub fn from_id(id: &str) -> Option<Self> {
            id.parse().ok()
        }
    }

    /// The order of a weapon's auction search.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SortBy {
        PriceAsc,
        PriceDesc,
    }

    names!(SortBy {
        PriceAsc => "price_asc",
        PriceDesc => "price_desc",
    });

    /// warframe.market's `rivenType` of a weapon, which is also what attributes' `exclusiveTo`
    /// lists name. Archguns report `rifle`; only `exclusiveTo` says `archgun`.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum RivenType {
        Rifle,
        Shotgun,
        Pistol,
        Kitgun,
        Melee,
        Zaw,
        Archgun,
        /// Anything warframe.market adds later.
        #[default]
        #[serde(other)]
        Unknown,
    }

    names!(RivenType {
        Rifle => "rifle",
        Shotgun => "shotgun",
        Pistol => "pistol",
        Kitgun => "kitgun",
        Melee => "melee",
        Zaw => "zaw",
        Archgun => "archgun",
        Unknown => "unknown",
    });

    /// warframe.market's weapon `group`, what the weapon list is organized by.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum WeaponGroup {
        Primary,
        Secondary,
        Melee,
        Archgun,
        Zaw,
        Kitgun,
        /// Companion weapons.
        Sentinel,
        /// Missing, or anything warframe.market adds later.
        #[default]
        #[serde(other)]
        Unknown,
    }

    names!(WeaponGroup {
        Primary => "primary",
        Secondary => "secondary",
        Melee => "melee",
        Archgun => "archgun",
        Zaw => "zaw",
        Kitgun => "kitgun",
        Sentinel => "sentinel",
        Unknown => "unknown",
    });

    /// Which column of the riven stat tables a weapon uses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum WeaponClass {
        Rifle,
        Shotgun,
        Pistol,
        Archgun,
        Melee,
    }

    names!(WeaponClass {
        Rifle => "rifle",
        Shotgun => "shotgun",
        Pistol => "pistol",
        Archgun => "archgun",
        Melee => "melee",
    });

    impl WeaponClass {
        /// Kitguns use pistol values, zaws use melee values, archguns have their own column
        /// even though warframe.market reports them as `rifle`.
        pub fn of(riven_type: RivenType, group: WeaponGroup) -> Option<Self> {
            if group == WeaponGroup::Archgun {
                return Some(Self::Archgun);
            }
            match riven_type {
                RivenType::Rifle => Some(Self::Rifle),
                RivenType::Shotgun => Some(Self::Shotgun),
                RivenType::Pistol | RivenType::Kitgun => Some(Self::Pistol),
                RivenType::Melee | RivenType::Zaw => Some(Self::Melee),
                RivenType::Archgun => Some(Self::Archgun),
                RivenType::Unknown => None,
            }
        }
    }

    /// The riven type an attribute's `exclusiveTo` list would name for this weapon.
    pub fn riven_kind(riven_type: RivenType, group: WeaponGroup) -> RivenType {
        if group == WeaponGroup::Archgun {
            RivenType::Archgun
        } else {
            riven_type
        }
    }

    /// Bows show their fire rate doubled, which the stat model needs to know. `game_ref` is the
    /// weapon's internal path.
    pub fn is_bow(riven_type: RivenType, game_ref: &str) -> bool {
        riven_type != RivenType::Melee && game_ref.to_ascii_lowercase().contains("bow")
    }

    /// How an attribute's value is shown.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum Unit {
        /// A change in percent, `+12.3%`.
        Percent,
        /// Faction damage, a factor: `x1.07`.
        Multiply,
        /// Seconds, meters, combo counts, as they are.
        Raw,
    }

    impl Unit {
        /// From an attribute's `unit` on warframe.market, which leaves it out for raw values.
        pub fn from_wfm(unit: Option<&str>) -> Self {
            match unit {
                Some("percent") => Self::Percent,
                Some("multiply") => Self::Multiply,
                _ => Self::Raw,
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Start a sweep over every weapon now (ignored while a sweep runs).
    ScrapeAll,
    /// Stop the running sweep after aborting the current request.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    /// Checking / downloading the weapon list.
    Metadata,
    /// Compacting closed Parquet partitions and pruning committed raw archives.
    Maintenance,
    Scraping,
    /// A fetch failed; waiting before the next one.
    Cooldown,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    Schedule,
    Manual,
    /// No longer started, but a sweep saved by an older version can still have it.
    Weapons,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurrentFetch {
    pub weapon: String,
    pub started_at: i64,
    /// e.g. "price_desc page", "verifying 3 auctions".
    pub step: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepProgress {
    pub started_at: i64,
    pub trigger: Trigger,
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    pub auctions: u32,
    pub gone: u32,
    pub current: Option<CurrentFetch>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepSummary {
    pub started_at: i64,
    pub finished_at: i64,
    pub trigger: Trigger,
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    pub auctions: u32,
    pub gone: u32,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FetchLog {
    pub t: i64,
    pub weapon: String,
    pub duration_ms: i64,
    pub pages: u32,
    pub auctions: u32,
    pub gone: u32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub sweep_interval_hours: f64,
    pub request_interval_secs: f64,
    pub sort_by: Vec<SortBy>,
    pub verify_missing: usize,
    pub weapons: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScraperStatus {
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub phase: Phase,
    pub sweep: Option<SweepProgress>,
    pub next_sweep_at: Option<i64>,
    pub last_sweep: Option<SweepSummary>,
    /// Newest first.
    pub recent: Vec<FetchLog>,
    /// Requests to warframe.market since the engine started.
    pub requests: u64,
    pub settings: Settings,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_match_serde() {
        for feed in Feed::ALL {
            assert_eq!(feed.as_str().parse::<Feed>(), Ok(*feed));
            assert_eq!(serde_json::to_value(feed).unwrap(), feed.as_str());
        }
        for order in SortBy::ALL {
            assert_eq!(order.as_str().parse::<SortBy>(), Ok(*order));
            assert_eq!(serde_json::to_value(order).unwrap(), order.as_str());
        }
        assert!("price".parse::<SortBy>().is_err());
        for class in WeaponClass::ALL {
            assert_eq!(class.as_str().parse::<WeaponClass>(), Ok(*class));
        }
        // New names from warframe.market must not break the weapon list.
        let group: WeaponGroup = serde_json::from_str("\"spacegun\"").unwrap();
        assert_eq!(group, WeaponGroup::Unknown);
        let riven_type: RivenType = serde_json::from_str("\"spacegun\"").unwrap();
        assert_eq!(WeaponClass::of(riven_type, group), None);
        assert_eq!(
            WeaponClass::of(RivenType::Rifle, WeaponGroup::Archgun),
            Some(WeaponClass::Archgun)
        );
    }

    #[test]
    fn user_agent_names_the_project_version_and_repository() {
        assert_eq!(
            super::USER_AGENT,
            format!(
                "warframe-market-tracker/{} (+https://github.com/LLukas22/warframe-market-tracker)",
                env!("CARGO_PKG_VERSION")
            )
        );
    }
}
