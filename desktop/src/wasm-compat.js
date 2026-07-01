// WKWebView (the webview Tauri uses on macOS) mishandles wasm that's inlined as a
// `data:application/wasm;base64,…` URL — which is exactly how
// @novasamatech/substrate-slot-sr25519-wasm ships the sr25519 slot-account signer
// behind the statement-store allowance. Two WKWebView bugs bite, in sequence:
//
//   1. `WebAssembly.instantiateStreaming` on a `data:` URL response throws
//      "Unexpected response MIME type. Expected 'application/wasm'".
//   2. Even via the fallback path, `fetch()` of a `data:` URL returns an EMPTY
//      body, so `instantiate(arrayBuffer)` fails with
//      "WebAssembly.Module doesn't parse at byte 0: expected a module of at least
//      8 bytes".
//
// So we (1) remove the streaming entry points → wasm-bindgen falls back to
// `WebAssembly.instantiate(arrayBuffer)`, and (2) shim `fetch` to decode `data:`
// URLs ourselves (returning a real Response with the actual bytes). Real http(s)
// requests pass straight through to the native fetch, so nothing else is affected.
// Imported FIRST in main.js, before any dependency touches WebAssembly or fetch.

if (typeof WebAssembly !== "undefined") {
  if (typeof WebAssembly.instantiateStreaming === "function") {
    WebAssembly.instantiateStreaming = undefined;
  }
  if (typeof WebAssembly.compileStreaming === "function") {
    WebAssembly.compileStreaming = undefined;
  }
}

if (typeof globalThis.fetch === "function") {
  const nativeFetch = globalThis.fetch.bind(globalThis);

  const urlOf = (input) => {
    if (typeof input === "string") return input;
    if (input instanceof URL) return input.href;
    if (input && typeof input.url === "string") return input.url; // Request
    return String(input);
  };

  const dataUrlToResponse = (url) => {
    const comma = url.indexOf(",");
    const meta = url.slice(5, comma); // strip leading "data:"
    const payload = url.slice(comma + 1);
    const isBase64 = /;base64\s*$/i.test(meta);
    const mime = meta.replace(/;base64\s*$/i, "") || "text/plain";
    let bytes;
    if (isBase64) {
      const bin = atob(payload);
      bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    } else {
      bytes = new TextEncoder().encode(decodeURIComponent(payload));
    }
    return new Response(bytes, { headers: { "Content-Type": mime } });
  };

  globalThis.fetch = function (input, init) {
    try {
      const url = urlOf(input);
      if (url && url.startsWith("data:")) {
        return Promise.resolve(dataUrlToResponse(url));
      }
    } catch (e) {
      /* fall through to native fetch on any parsing hiccup */
    }
    return nativeFetch(input, init);
  };
}
