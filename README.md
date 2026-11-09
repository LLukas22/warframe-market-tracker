# Warframe Market Tracker

Tracks [warframe.market](https://warframe.market) riven auctions so you can search listings, follow prices, and compare stats. Paste a riven screenshot into the search bar to scan it locally in your browser.

With Docker installed, copy `.env.example` to `.env`, adjust the settings, and run `mkdir -p data && docker compose up -d` from this repository. Open `http://localhost:8080` (or your configured `PORT`); set `ADMIN_TOKEN` in `.env` to enable manual scraping and data imports/exports.
