#!/bin/sh
# Runs the dev server on a fixed port (PORT, default 8081), with data downloaded from the public
# Hugging Face dataset like a default self-hosted server. Set WFMT_HF_PULL_DATASET= (empty) to use
# only the local data. Extra arguments go to `dx serve`.
set -eu
cd "$(dirname "$0")/.."
export WFMT_HF_PULL_DATASET="${WFMT_HF_PULL_DATASET-LLukas22/wf-riven-data}"
exec dx serve --port "${PORT:-8081}" "$@"
