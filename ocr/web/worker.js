// Runs the riven scanner off the main thread; a scan blocks for a few hundred ms. Started by
// scanner.js, which is what pages use.
// Adapted from pure-onnx-ocr's examples/web/worker.js (Apache-2.0,
// https://github.com/siska-tech/pure-onnx-ocr).
import init, { RivenScanner } from "./pkg/wfmt_ocr.js";

let scanner = null;

// Low priority so these never compete with the page's own requests.
async function fetchOk(url) {
  const response = await fetch(url, { priority: "low" });
  if (!response.ok) throw new Error(`failed to fetch ${url}: ${response.status}`);
  return response;
}
const bytes = async (url) => new Uint8Array(await (await fetchOk(url)).arrayBuffer());
const text = async (url) => (await fetchOk(url)).text();

self.onmessage = async ({ data }) => {
  const { id } = data;
  try {
    if (data.type === "load") {
      const started = performance.now();
      const model = (part, file) => `${data.models}/${data.tier}_${part}/${file}`;
      const [, detOnnx, detYml, recOnnx, recYml, weapons, attributes] = await Promise.all([
        // A Response promise, so the module download is low priority too.
        init({ module_or_path: fetch(new URL("./pkg/wfmt_ocr_bg.wasm", import.meta.url), { priority: "low" }) }),
        bytes(model("det", "inference.onnx")),
        text(model("det", "inference.yml")),
        bytes(model("rec", "inference.onnx")),
        text(model("rec", "inference.yml")),
        text(data.weapons),
        text(data.attributes),
      ]);
      scanner = new RivenScanner(detOnnx, detYml, recOnnx, recYml, weapons, attributes);
      self.postMessage({ id, type: "loaded", ms: performance.now() - started });
    } else if (data.type === "scan") {
      const started = performance.now();
      const scan = JSON.parse(scanner.scan(new Uint8Array(data.image)));
      self.postMessage({ id, type: "scanned", scan, ms: performance.now() - started });
    }
  } catch (error) {
    self.postMessage({ id, type: "error", message: String(error?.message ?? error) });
  }
};
