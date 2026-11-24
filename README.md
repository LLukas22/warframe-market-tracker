# Warframe Market Tracker

Tracks [warframe.market](https://warframe.market) riven auctions so you can search listings, follow prices, and compare stats. Paste a riven screenshot into the search bar to scan it locally in your browser. A riven's page checks whether a listing that isn't known to be gone is still on warframe.market; the server asks at most every 15 minutes per listing, or when you press the refresh button next to the answer, and keeps the answer in `checks.jsonl` in the data folder. A scraping server also adds it to its dataset.

The data is published as the Hugging Face dataset [LLukas22/wf-riven-data](https://huggingface.co/datasets/LLukas22/wf-riven-data), with a folder for each finished UTC day.

## Riven grades

Listings and screenshot scans show a letter for each fitted stat. **B** is near the midpoint of its possible range. A favorable deviation of 7.5% earns **A+**, and 9.5% earns **S**. For a negative attribute, a milder penalty earns a higher grade. The tracker uses its existing rank and disposition fit, so grades also work for fitted historical listings. [Altair grading chart](https://empx.cc/docs/altair/grading/)

## Self-hosting

With Docker installed, copy `.env.example` to `.env`, adjust the settings, and run `mkdir -p data && docker compose up -d` from this repository. Open `http://localhost:8080` (or your configured `PORT`). Set `ADMIN_TOKEN` in `.env` to enable the buttons on the scraper page, such as checking for new data and importing or exporting it.

By default the server doesn't scrape. It downloads its data from [LLukas22/wf-riven-data](https://huggingface.co/datasets/LLukas22/wf-riven-data), which is set as `HF_PULL_DATASET` in `.env.example`. The first download happens when it starts. After that it checks for new days every hour and fetches only what changed, and the scraper page shows the last update and can check right away. Days the dataset doesn't have stay as they are. To use another dataset, set its `owner/name`; a private one also needs a read token in `HF_TOKEN`.

## Scraping it yourself

To collect the data yourself instead, set `ENABLE_SCRAPER=true`. A scraping server has its own data, so it ignores `HF_PULL_DATASET` and stops downloading. The scraper fetches every weapon's auctions every few hours and follows the site's newest listings in between. With `ADMIN_TOKEN` set, you can also start or cancel a scrape from the scraper page.

To publish what you scrape, create a Hugging Face dataset and set `HF_DATASET` (`owner/name`) and a write token in `HF_TOKEN`. Each finished UTC day is then uploaded once it's compacted, adding only that day's files, and the dataset gets a README describing the tables unless you've written one yourself. The scraper page shows the last upload and can start one right away.

## Code layout

A Cargo workspace. The web app (Dioxus, at the root) is a thin layer over library crates; each crate depends only on those above it:

| Crate | What it holds |
| --- | --- |
| `types/` (`wfmt-types`) | Shared names: weapon types, feeds, sort orders, the attribute table, scraper status. Builds for the browser. |
| `api/` (`wfmt-api`) | The queries and answers the browser and server exchange. Builds for the browser. |
| `model/` (`wfmt-model`) | The riven stat model: fits listed values to a rank, a roll and a disposition. |
| `dataset/` (`wfmt-dataset`) | The data on disk: layout, parquet schema, transactional writes, compaction, the riven registry. |
| `scraper/` (`wfmt-scraper`) | The warframe.market client and the engine that turns its responses into dataset rows. |
| `store/` (`wfmt-store`) | Listing summaries in RAM. Embedded DuckDB reads historical prices directly from the scraper’s Parquet files for charts and riven journeys. |
| `ocr/` (`wfmt-ocr`) | Reads rivens from screenshots, in the browser. |

## Development

- `scripts/dev.sh` runs the dev server on port 8081 (or `PORT`), with data downloaded from the Hugging Face dataset into `./data`. Set `WFMT_HF_PULL_DATASET=` to use only the local data.
- `cargo test --workspace --features server` runs the tests. `store/tests/snapshots.rs` compares the answers to every kind of query with the snapshots in `store/tests/snapshots/`; after an intended change, review and accept the new ones with [`cargo insta review`](https://insta.rs).
- `cargo bench -p wfmt-store --bench queries` times the queries on the real dataset in `./data` (or `WFMT_DATA_DIR`).
- `WFMT_GOLDEN=out.jsonl cargo test --release -p wfmt-store golden -- --ignored` writes the answers to many queries on the real dataset, to diff two versions of the code.
- `cargo run -p wfmt-scraper --release --features reprocess --bin reprocess -- --replace` rebuilds a stopped server's dataset from its raw archive.

## People

Open **People** for everyone seen selling rivens, sortable by auctions, rivens listed now,
resales, rivens sold on, reputation, or last sighting, and searchable by name. Counts cover the
period you pick, with the market page's presets or a custom range of UTC days: auctions listed
during it, and rivens that changed hands during it. "Listed now" is always current. A resale is a
riven that another seller listed before them: rivens keep their id across auctions, so a new
seller on the same riven most likely bought it, though warframe.market doesn't confirm trades. A
rerolled riven counts as a new one. Each seller's page lists their rivens, with who listed each
one before and after them. Seller names on listings and riven pages link there.

## In-game trade prices

Every Monday, Digital Extremes publishes what rivens sold for in in-game trades over the past
week, for each platform: per weapon, rerolled or not, the median, average, spread, range and a
popularity score from 1 to 100. Listings on warframe.market are asking prices, these were paid.
A weapon's price chart draws the PC medians as two faint lines under the asking prices, solid
for rivens never rerolled and dashed for rerolled ones, each report from the week it covers until
the next one's. The report gives no trade counts, only a popularity relative to the most traded
riven: adding up a weapon's two, scaled so the most traded weapon is 100, gives its in-game
popularity, shown on its card and page, and the weapons can be sorted by it.

DE only ever publishes the current week, so the tracker keeps them. Every server checks the
reports every 6 hours and saves each new week in `weekly_rivens/<platform>/<date>.parquet` in
the data folder, with the response as it came in `weekly_rivens/raw/`. An uploading server adds
the weeks to its Hugging Face dataset as the `weekly_rivens` table, and a pulling server downloads
the weeks it doesn't have, so it gets the history from before it started. The scraper page shows
the newest week and the last check.

## Browser alerts

Open **Alerts** to save a weapon, maximum platinum price, seller status, and minimum seller
reputation. Rules and per-listing notification history are saved only in this browser's local
storage. Pause/resume or delete rules from the tab. Enable browser notifications when prompted;
HTTPS (or localhost) is required, and blocked permissions must be changed in browser site settings.

Keep any tracker page open. It checks the server's shared data every 30 seconds and notifies once
per rule and matching fixed-price listing, including existing matches on the first check. Bidding
starting prices do not trigger alerts. Matches also appear in the Alerts tab if notifications are
unavailable. Only listings observed in the past 15 minutes are eligible; seller status and
reputation reflect that observation. Live scraping is needed for timely alerts: the default
Hugging Face downloads are delayed, and background tabs may be throttled by the browser.

Run the browser alert controller tests with `node --test assets/alerts.test.cjs`.
