# Warframe Market Tracker

Tracks [warframe.market](https://warframe.market) riven auctions so you can search listings, follow prices, and compare stats. Paste a riven screenshot into the search bar to scan it locally in your browser.

With Docker installed, copy `.env.example` to `.env`, adjust the settings, and run `mkdir -p data && docker compose up -d` from this repository. Open `http://localhost:8080` (or your configured `PORT`); set `ADMIN_TOKEN` in `.env` to enable manual scraping and data imports/exports.

To publish the data, create a Hugging Face dataset and set `HF_DATASET` (`owner/name`) and a write token in `HF_TOKEN`. Each finished UTC day is then uploaded once it's compacted, adding only that day's files, and the dataset gets a README describing the tables unless you've written one yourself; the scraper page shows the last upload and can start one right away.
