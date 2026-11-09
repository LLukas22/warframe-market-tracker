// Lazy entry point to the riven scanner: the only file a page imports.
//
// Importing it downloads nothing and starts nothing. The worker, the 4.5 MB WebAssembly module
// and the models are fetched on the first `preload()` or `scan()`, and all the work happens in a
// Web Worker, so neither page load nor the UI ever waits on OCR. Call `preload()` when the user
// opens the upload dialog, so the download overlaps with them picking a screenshot.
//
//   const { createRivenScanner } = await import("/ocr/scanner.js");
//   const scanner = createRivenScanner({ models: "/ocr/models", weapons: "...", attributes: "..." });
//   scanner.preload();
//   const { riven, lines, ms } = await scanner.scan(file);

/**
 * @param {object} options
 * @param {string} options.models     Directory with `<tier>_det/` and `<tier>_rec/` model folders.
 * @param {string} options.weapons    URL of warframe.market's `/v2/riven/weapons` body.
 * @param {string} options.attributes URL of warframe.market's `/v2/riven/attributes` body.
 * @param {"tiny"|"small"} [options.tier] PP-OCRv6 model size, `tiny` by default.
 * @param {(state: "loading"|"ready"|"failed", detail?: {ms: number}|Error) => void} [options.onState]
 */
export function createRivenScanner({ models, weapons, attributes, tier = "tiny", onState = () => {} }) {
  let worker = null;
  let ready = null;
  let nextId = 0;
  const pending = new Map();

  function call(message, transfer = []) {
    const id = nextId++;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      worker.postMessage({ ...message, id }, transfer);
    });
  }

  /** Starts downloading and loading. Safe to call any number of times; resolves when ready. */
  function preload() {
    if (ready) return ready;
    // Resolve relative URLs here: the worker resolves them against its own location.
    const url = (path) => new URL(path, location.href).href;
    worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
    worker.onmessage = ({ data }) => {
      const request = pending.get(data.id);
      if (!request) return;
      pending.delete(data.id);
      data.type === "error" ? request.reject(new Error(data.message)) : request.resolve(data);
    };
    worker.onerror = (event) => {
      for (const request of pending.values()) request.reject(new Error(event.message || "OCR worker failed"));
      pending.clear();
    };
    const loading = worker;
    onState("loading");
    ready = call({ type: "load", tier, models: url(models), weapons: url(weapons), attributes: url(attributes) }).then(
      (loaded) => {
        onState("ready", { ms: loaded.ms });
        return loaded;
      },
      (error) => {
        // Let a later call try again, e.g. after a network error, unless dispose() or a newer
        // load already replaced this worker.
        if (worker === loading) {
          worker.terminate();
          worker = null;
          ready = null;
        }
        onState("failed", error);
        throw error;
      },
    );
    return ready;
  }

  /**
   * Scans a screenshot of one riven card. Loads the scanner first if needed.
   * @param {Blob|ArrayBuffer} image Encoded image (PNG, JPEG, ...).
   * @returns {Promise<{riven: object, lines: object[], ms: number}>}
   */
  async function scan(image) {
    await preload();
    const buffer = image instanceof Blob ? await image.arrayBuffer() : image;
    const { scan, ms } = await call({ type: "scan", image: buffer }, [buffer]);
    return { ...scan, ms };
  }

  /** Stops the worker and frees its memory (a few hundred MB once loaded). */
  function dispose() {
    worker?.terminate();
    worker = null;
    ready = null;
    for (const request of pending.values()) request.reject(new Error("scanner disposed"));
    pending.clear();
  }

  return { preload, scan, dispose, tier };
}
