// Boot Schist in the browser.
//
// The app's wasm is served split into fixed-size chunks (tools/web-build.sh
// cuts them; manifest.json lists them in order) so a ~20 MB module downloads
// over parallel connections with a byte-accurate progress bar, instead of as
// one opaque stall. The icons and fonts the app would natively embed are
// fetched here too — the wasm carries no assets — and handed over on
// `window.__schist_boot` before the module is instantiated, so the Rust side
// reads them synchronously and never needs an async asset path.
//
// The app calls `__schistLoadingDone` once its window is up, and its panic
// hook calls `__schistLoadingFailed`, so a crash during boot reads as an
// error, never as an eternally full progress bar.

const overlay = document.getElementById("schist-loading");
const fill = document.getElementById("schist-loading-fill");
const status = document.getElementById("schist-loading-status");

window.__schistLoadingDone = () => {
  overlay.classList.add("done");
  // Gone entirely once the fade finishes, so it can't sit over the canvas.
  setTimeout(() => overlay.remove(), 400);
};

window.__schistLoadingFailed = (message) => {
  if (!overlay.isConnected) return;
  overlay.classList.remove("done");
  status.remove();
  document.querySelector("#schist-loading .bar")?.remove();
  let card = document.getElementById("schist-loading-error");
  if (!card) {
    card = document.createElement("div");
    card.className = "error";
    card.id = "schist-loading-error";
    overlay.appendChild(card);
  }
  card.textContent = message;
};

function setStatus(text) {
  status.textContent = text;
}

// One shared progress count across every parallel fetch.
let totalBytes = 0;
let gotBytes = 0;
function onBytes(n) {
  gotBytes += n;
  if (totalBytes > 0) {
    fill.style.width = `${Math.min(100, (100 * gotBytes) / totalBytes)}%`;
  }
}

// Fetch one file, streaming so the bar moves per chunk received rather
// than per file completed.
async function fetchBytes(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`HTTP ${response.status} for ${url}`);
  if (!response.body) {
    const buffer = new Uint8Array(await response.arrayBuffer());
    onBytes(buffer.length);
    return buffer;
  }
  const reader = response.body.getReader();
  const parts = [];
  let length = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value);
    length += value.length;
    onBytes(value.length);
  }
  const out = new Uint8Array(length);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

async function boot() {
  if (!navigator.gpu) {
    throw new Error(
      "Schist needs WebGPU, which this browser doesn't offer. " +
        "Chrome/Edge 113+, Firefox 141+ and Safari 26+ do.",
    );
  }

  setStatus("Loading…");
  const manifest = await (await fetch("manifest.json")).json();
  totalBytes =
    manifest.wasm.reduce((n, c) => n + c.bytes, 0) +
    manifest.fonts.reduce((n, f) => n + f.bytes, 0) +
    manifest.assets.reduce((n, a) => n + a.bytes, 0);

  // Everything in parallel: the wasm chunks dominate, and the browser
  // pools the connections.
  const wasmChunks = Promise.all(manifest.wasm.map((c) => fetchBytes(c.file)));
  const fonts = Promise.all(manifest.fonts.map((f) => fetchBytes(f.file)));
  const assets = Promise.all(
    manifest.assets.map(async (a) => [a.path, await fetchBytes(a.file)]),
  );

  const chunks = await wasmChunks;
  const wasm = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const chunk of chunks) {
    wasm.set(chunk, at);
    at += chunk.length;
  }

  window.__schist_boot = {
    fonts: await fonts,
    assets: Object.fromEntries(await assets),
  };

  setStatus("Starting…");
  const { default: init } = await import(`./${manifest.js}`);
  await init({ module_or_path: wasm });
  // From here the app owns the page; __schistLoadingDone fires once its
  // window is up and painting.
}

boot().catch((err) => {
  console.error(err);
  window.__schistLoadingFailed(String(err?.message ?? err));
});
