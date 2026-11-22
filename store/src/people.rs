//! Sellers: what each one listed, and which rivens passed from one seller to another.
//!
//! A riven keeps its id across auctions (see `wfmt_dataset::identity`), so its auctions in
//! order of [`recency`] show who held it when. Each change of seller is taken as a trade: the
//! later seller resold a riven they got from the earlier one. Rivens whose stats couldn't be
//! fitted get their seller mixed into the id, so they never change hands here.
//!
//! The counts are for a period: an auction counts if it was listed at some point in it, a
//! resale if the riven came to the seller in it. Who held a riven before is known from the
//! whole history, so a resale is found even when the earlier listing predates the period.

use super::load::set_str;
use super::*;
use std::cmp::Reverse;
use std::sync::Arc;
use wfmt_api::{
    Page, PeopleQuery, PeopleResult, PeopleSort, Period, Person, PersonDetail, PersonFilter,
    PersonRiven,
};

/// Everyone's rivens, built once per data version (see `Memo`). The counts in `people` are
/// left at zero: they depend on the period, see [`Data::counts`].
#[derive(Default)]
pub(super) struct People {
    people: Vec<Person>,
    /// `people[i].name` in lowercase, for the search.
    keys: Vec<String>,
    /// Lowercase name to index in `people`. warframe.market names ignore case.
    by_name: HashMap<String, usize>,
    /// Per person, the rivens they listed.
    held: Vec<Vec<Held>>,
}

/// One riven one person listed.
struct Held {
    /// The auction that stands for it: the riven's current listing if that's theirs, else
    /// their newest auction of it.
    auction: AuctionId,
    /// They list it right now: the riven's newest current auction is theirs.
    current: bool,
    /// Their auctions of it.
    auctions: Vec<AuctionId>,
    /// Each time it came to them from another seller: when they first listed it then, and
    /// the other seller's last auction of it. Oldest first.
    bought: Vec<(i64, AuctionId)>,
    /// Each time it went from them to another seller: when that seller first listed it, and
    /// their first auction of it. Oldest first.
    sold: Vec<(i64, AuctionId)>,
}

/// From when to when an auction was listed, as far as anyone saw.
fn listed(a: &Auction) -> (i64, i64) {
    (a.created.min(a.first_seen), a.seen())
}

/// A period's half-open `[start, end)` bounds, `None` unbounded.
type Bounds = (Option<i64>, Option<i64>);

fn overlaps((start, end): Bounds, (from, to): (i64, i64)) -> bool {
    start.is_none_or(|s| to >= s) && end.is_none_or(|e| from < e)
}

fn within((start, end): Bounds, t: i64) -> bool {
    start.is_none_or(|s| t >= s) && end.is_none_or(|e| t < e)
}

/// What one seller did in a period.
#[derive(Clone, Copy, Default)]
struct Counts {
    auctions: u32,
    rivens: u32,
    active: u32,
    resales: u32,
    sold_on: u32,
}

impl Counts {
    /// They did something in the period. Listing now isn't enough: their listing might
    /// have started after a past period.
    fn any(&self) -> bool {
        self.auctions > 0 || self.sold_on > 0
    }

    fn apply(self, person: &mut Person) {
        person.auctions = self.auctions;
        person.rivens = self.rivens;
        person.active = self.active;
        person.resales = self.resales;
        person.sold_on = self.sold_on;
    }
}

impl Held {
    /// Still listed, and listed in the period too: a listing nobody has seen since before
    /// the period isn't what they're selling in it, though it was never found gone.
    fn listed_now(&self, data: &Data, bounds: Bounds) -> bool {
        self.current && overlaps(bounds, listed(&data.auctions[self.auction]))
    }

    fn listed_in(&self, data: &Data, bounds: Bounds) -> usize {
        self.auctions
            .iter()
            .filter(|&&i| overlaps(bounds, listed(&data.auctions[i])))
            .count()
    }

    fn bought_in(&self, bounds: Bounds) -> bool {
        self.bought.iter().any(|&(t, _)| within(bounds, t))
    }

    fn sold_in(&self, bounds: Bounds) -> bool {
        self.sold.iter().any(|&(t, _)| within(bounds, t))
    }
}

/// Marks auctions without a seller name.
const NOBODY: usize = usize::MAX;

impl Data {
    /// [`People`], reused for as long as the data doesn't change. Nothing in it depends on
    /// the clock.
    fn people_index(&self) -> Arc<People> {
        // A memo is only ever replaced whole, so one left by a panic is still consistent.
        let mut memo = self
            .memo
            .people
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((version, people)) = &*memo
            && *version == self.version
        {
            return people.clone();
        }
        let people = Arc::new(self.build_people());
        *memo = Some((self.version, people.clone()));
        people
    }

    fn build_people(&self) -> People {
        let mut index = People::default();
        let mut owner = self.auctions.same_ids(NOBODY);
        for (id, a) in self.auctions.enumerate() {
            let name = a.owner_name.trim();
            if name.is_empty() {
                continue;
            }
            let key = name.to_lowercase();
            let p = match index.by_name.get(&key) {
                Some(&p) => p,
                None => {
                    index.by_name.insert(key.clone(), index.people.len());
                    index.keys.push(key);
                    index.people.push(Person {
                        name: String::new(),
                        auctions: 0,
                        rivens: 0,
                        active: 0,
                        resales: 0,
                        sold_on: 0,
                        reputation: None,
                        platform: String::new(),
                        last_seen: i64::MIN,
                        first_listed: i64::MAX,
                    });
                    index.people.len() - 1
                }
            };
            owner[id] = p;
            let person = &mut index.people[p];
            person.first_listed = person.first_listed.min(a.created);
            // Name, reputation and platform as of their latest sighting.
            if a.seen() >= person.last_seen {
                person.last_seen = a.seen();
                set_str(&mut person.name, name);
                set_str(&mut person.platform, &a.platform);
                person.reputation = a.owner_reputation;
            }
        }

        index.held = (0..index.people.len()).map(|_| Vec::new()).collect();
        let newest = self.newest_current(&self.rows());
        let mut order = Vec::new();
        // Consecutive auctions of one seller: (seller, first auction, last auction).
        let mut runs: Vec<(usize, AuctionId, AuctionId)> = Vec::new();
        // Per seller in `runs`, where their entry in `held` is.
        let mut slots: Vec<(usize, usize)> = Vec::new();
        for (riven, auctions) in self.riven_auctions.enumerate() {
            order.clear();
            order.extend(auctions.iter().copied().filter(|&i| owner[i] != NOBODY));
            order.sort_by(|&x, &y| recency(&self.auctions[x]).cmp(&recency(&self.auctions[y])));
            runs.clear();
            for &i in &order {
                match runs.last_mut() {
                    Some(run) if run.0 == owner[i] => run.2 = i,
                    _ => runs.push((owner[i], i, i)),
                }
            }
            let current = newest[riven].filter(|&n| owner[n] != NOBODY);
            slots.clear();
            for (k, &(p, first, last)) in runs.iter().enumerate() {
                let slot = match slots.iter().find(|(q, _)| *q == p) {
                    Some(&(_, slot)) => slot,
                    None => {
                        let held = &mut index.held[p];
                        held.push(Held {
                            auction: last,
                            current: false,
                            auctions: Vec::new(),
                            bought: Vec::new(),
                            sold: Vec::new(),
                        });
                        slots.push((p, held.len() - 1));
                        held.len() - 1
                    }
                };
                let held = &mut index.held[p][slot];
                held.auction = last;
                if k > 0 {
                    held.bought
                        .push((listed(&self.auctions[first]).0, runs[k - 1].2));
                }
                if let Some(&(_, next, _)) = runs.get(k + 1) {
                    held.sold.push((listed(&self.auctions[next]).0, next));
                }
            }
            for &i in &order {
                let p = owner[i];
                if let Some(&(_, slot)) = slots.iter().find(|(q, _)| *q == p) {
                    index.held[p][slot].auctions.push(i);
                }
            }
            if let Some(n) = current {
                let p = owner[n];
                if let Some(&(_, slot)) = slots.iter().find(|(q, _)| *q == p) {
                    let held = &mut index.held[p][slot];
                    held.auction = n;
                    held.current = true;
                }
            }
        }
        index
    }

    /// What one seller did in a period.
    fn counts(&self, held: &[Held], bounds: Bounds) -> Counts {
        let mut counts = Counts::default();
        for h in held {
            let listed = h.listed_in(self, bounds) as u32;
            counts.auctions += listed;
            counts.rivens += u32::from(listed > 0);
            counts.active += u32::from(h.listed_now(self, bounds));
            counts.resales += u32::from(h.bought_in(bounds));
            counts.sold_on += u32::from(h.sold_in(bounds));
        }
        counts
    }

    /// A page of the sellers active in `q.period` whose name contains `q.search`, in `q.sort`
    /// order.
    pub fn people(&self, q: &PeopleQuery, page: Page, now: i64) -> PeopleResult {
        let index = self.people_index();
        let bounds = q.period.bounds(now);
        let counts: Vec<Counts> = index.held.iter().map(|h| self.counts(h, bounds)).collect();
        let active: Vec<usize> = (0..index.people.len())
            .filter(|&i| counts[i].any())
            .collect();
        let everyone = active.len();
        let needle = q.search.trim().to_lowercase();
        let mut matches: Vec<usize> = active
            .into_iter()
            .filter(|&i| needle.is_empty() || index.keys[i].contains(&needle))
            .collect();
        let people = &index.people;
        let by = |i: usize| -> (i64, i64) {
            let (c, p) = (&counts[i], &people[i]);
            match q.sort {
                PeopleSort::Auctions => (c.auctions.into(), c.active.into()),
                PeopleSort::Active => (c.active.into(), c.auctions.into()),
                PeopleSort::Resales => (c.resales.into(), c.auctions.into()),
                PeopleSort::SoldOn => (c.sold_on.into(), c.auctions.into()),
                // Unknown reputations go last.
                PeopleSort::Reputation => (p.reputation.unwrap_or(-1), c.auctions.into()),
                PeopleSort::LastSeen => (p.last_seen, c.auctions.into()),
            }
        };
        matches.sort_by(|&a, &b| {
            Reverse(by(a))
                .cmp(&Reverse(by(b)))
                .then_with(|| index.keys[a].cmp(&index.keys[b]))
        });
        PeopleResult {
            total: matches.len(),
            everyone,
            people: matches
                .into_iter()
                .skip(page.offset)
                .take(page.limit.min(wfmt_api::PEOPLE_MAX_ROWS))
                .map(|i| {
                    let mut person = people[i].clone();
                    counts[i].apply(&mut person);
                    person
                })
                .collect(),
            server_time: now,
        }
    }

    /// One seller's totals in `period` and a page of their rivens, `None` for a name never
    /// seen. Who a riven came from and went to is the latest handover, whenever it was.
    pub fn person(
        &self,
        name: &str,
        filter: PersonFilter,
        period: Period,
        page: Page,
        now: i64,
    ) -> Option<PersonDetail> {
        let index = self.people_index();
        let &p = index.by_name.get(&name.trim().to_lowercase())?;
        let bounds = period.bounds(now);
        let mut held: Vec<&Held> = index.held[p]
            .iter()
            .filter(|h| match filter {
                PersonFilter::Active => h.listed_now(self, bounds),
                PersonFilter::All => h.listed_in(self, bounds) > 0,
                PersonFilter::Resales => h.bought_in(bounds),
                PersonFilter::SoldOn => h.sold_in(bounds),
            })
            .collect();
        held.sort_by(|a, b| {
            b.current.cmp(&a.current).then_with(|| {
                recency(&self.auctions[b.auction]).cmp(&recency(&self.auctions[a.auction]))
            })
        });
        let seller = |handovers: &[(i64, AuctionId)]| {
            handovers
                .last()
                .map(|&(_, i)| self.auctions[i].owner_name.clone())
        };
        let mut person = index.people[p].clone();
        self.counts(&index.held[p], bounds).apply(&mut person);
        Some(PersonDetail {
            person,
            total: held.len(),
            rivens: held
                .into_iter()
                .skip(page.offset)
                .take(page.limit.min(wfmt_api::PEOPLE_MAX_ROWS))
                .map(|h| PersonRiven {
                    listing: self.listing(&self.auctions[h.auction]),
                    bought_from: seller(&h.bought),
                    sold_to: seller(&h.sold),
                })
                .collect(),
            server_time: now,
        })
    }
}
