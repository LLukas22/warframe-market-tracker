# Warframe Market Tracker

Tracks [warframe.market](https://warframe.market) riven auctions so you can search listings, follow prices, and compare stats. Paste a riven screenshot into the search bar to scan it locally in your browser.

The data is published as the Hugging Face dataset [LLukas22/wf-riven-data](https://huggingface.co/datasets/LLukas22/wf-riven-data), with a folder for each finished UTC day.

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
| `store/` (`wfmt-store`) | The server's read model: the dataset in memory, answering searches and riven pages. |
| `ocr/` (`wfmt-ocr`) | Reads rivens from screenshots, in the browser. |
