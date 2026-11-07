# Warframe Market Tracker

Warframe Market Tracker scrapes [warframe.market](https://warframe.market) riven auctions in the background every 4 hours and stores the normalized listings as parquet files.
Its web UI lets you search rivens, follow their prices over time, and see which stats each weapon's sellers charge more for.
Listings with a positive buyout price or an actual positive bid enter the normalized dataset. Starting-price-only listings qualify only above the arithmetic mean of the other fetched listings’ positive buyout or bid prices for that weapon (one price per unique auction, preferring buyout). Starting-price-only listings are excluded when no reference prices are available; 1-platinum placeholders are always excluded. When auctions without buyouts crowd the capped expensive-first page, the scraper also requests direct sales and auctions with buyouts separately. Raw responses retain all returned listings for replay and disappearance tracking.

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
