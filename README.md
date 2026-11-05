# Warframe Market Tracker

Warframe Market Tracker scrapes [warframe.market](https://warframe.market) riven auctions in the background every 4 hours and stores the normalized listings as parquet files.
Its web UI lets you search rivens, follow their prices over time, and see which stats each weapon's sellers charge more for.

## Deploy

```sh
cp .env.example .env    # set DATA_DIR, PORT and ADMIN_TOKEN here
mkdir -p data           # or the folder you set as DATA_DIR
docker compose up -d --build
```

The UI is then at `http://localhost:8080`, or on whichever `PORT` you set.
The scraper page needs `ADMIN_TOKEN` to start or cancel a scrape, and to move your data to a server with Export and Upload.
