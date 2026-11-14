//! Scrapes warframe.market's riven auctions into the dataset (see `wfmt_dataset`): the API
//! client, the engine that schedules fetches, turning responses into rows, and telling which
//! listings disappeared. A library only: the server is what runs the scraper.

pub mod archive;
pub mod engine;
pub mod normalize;
pub mod pipeline;
pub mod reprocess;
pub mod tracking;
pub mod wfm;
