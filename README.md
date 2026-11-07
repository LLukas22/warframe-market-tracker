# Warframe Market Tracker

Warframe Market Tracker scrapes [warframe.market](https://warframe.market) riven auctions in the background every 4 hours and stores the normalized listings as parquet files.
Its web UI lets you search rivens, follow their prices over time, and see which stats each weapon's sellers charge more for.
Click a riven to open a shareable detail page with an interactive price chart, first/last sightings, seller intervals, and similar rivens from the stored dataset. Riven ID searches lead to the same detail page; normalized stats and comparison settings are tucked into expandable sections. Comparisons show at most 10 distinct rivens in one list, ranked by shared positive stats, fewer added stats, matching negatives and stat counts, then normalized roll closeness. Two shared positives qualify by default; an explicit broadening option includes one-stat matches. Relistings appear once; prices are last observed asking prices or bids, not confirmed sales. Pre-rework and unverified comparisons can be included explicitly.

Listings with a positive buyout price or an actual positive bid enter the normalized dataset. Starting-price-only listings qualify only above the arithmetic mean of the other fetched listings’ positive buyout or bid prices for that weapon (one price per unique auction, preferring buyout). Starting-price-only listings are excluded when no reference prices are available; 1-platinum placeholders are always excluded. When auctions without buyouts crowd the capped expensive-first page, the scraper also requests direct sales and auctions with buyouts separately. Raw responses retain all returned listings for replay and disappearance tracking.

Between sweeps the scraper also polls the site-wide "most recent" feed every 3 minutes and the "popular" feed every 10 minutes (`WFMT_RECENT_INTERVAL_SECS` and `WFMT_POPULAR_INTERVAL_SECS`, 0 disables). Feeds pick up new listings and price changes quickly, but they only prove a listing exists: a listing's absence from a feed never marks it as gone, and a sighting doesn't count as a scrape of its weapon. Starting-price-only listings from feeds wait for the next weapon search. Each listing gets a row when it first appears, whenever its price, bid or `updated` time changes, and at most hourly while a feed keeps returning it unchanged. Raw feed responses are kept for 6 hours (`WFMT_FEED_RAW_RETENTION_HOURS`, 0 keeps them as long as other raw responses); their normalized rows stay. Once any have been deleted, `reprocess --replace` refuses to run because those rows can't be rebuilt; `--out` still works.

## Deploy

```sh
curl -O https://raw.githubusercontent.com/LLukas22/warframe-market-tracker/main/compose.yaml
curl -o .env https://raw.githubusercontent.com/LLukas22/warframe-market-tracker/main/.env.example
# set DATA_DIR, PORT and ADMIN_TOKEN in .env
mkdir -p data           # or the folder you set as DATA_DIR
docker compose up -d
```

The UI is then at `http://localhost:8080`, or on whichever `PORT` you set.
To update, run `docker compose pull && docker compose up -d`.
The scraper page needs `ADMIN_TOKEN` to start or cancel a scrape, and to move your data to a server with Export and Upload.
