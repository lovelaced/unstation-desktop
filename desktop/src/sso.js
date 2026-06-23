// Real Polkadot-app sign-in (SSO) via @novasamatech/host-papp, running in the
// webview. Pairing happens through the Paseo statement store: the desktop posts a
// pairing proposal, shows the QR (`pairingStatus.payload`), the phone scans +
// approves, and a UserSession is established. The session's slot keys then sign
// statement-store / Bulletin writes locally (keys stay on the phone).
//
// Live pairing requires the Polkadot mobile app + Paseo reachability, so it runs
// only inside the Tauri webview (the browser preview keeps the mock onboarding).

import { createPappAdapter } from "@novasamatech/host-papp";
import { createLazyClient, createPapiStatementStoreAdapter } from "@novasamatech/statement-store";
import { getWsProvider } from "@polkadot-api/ws-provider";
import { onHostPappDebugMessage } from "@novasamatech/host-papp/debug";
// For extracting the paired statement-store slot signing key (see
// statementStoreSlotKey) — same primitives host-papp uses to encrypt it at rest.
import { blake2b } from "@noble/hashes/blake2.js";
import { gcm } from "@noble/ciphers/aes.js";

// MUST match the `appId` passed to createPappAdapter below — it's both the
// host-papp storage prefix and the AES salt for the allowance blob.
const APP_ID = "unstation";

// The desktop MUST subscribe to the SAME statement-store (People) chain the
// Polkadot app posts its pairing handshake to. If they differ, the phone links
// successfully but the desktop never sees the response statement — the link just
// silently never completes.
//
// CAUTION: do NOT use host-papp's exported `SS_PASEO_STABLE_STAGE_ENDPOINTS` — it
// points at `wss://paseo-people-next-rpc.polkadot.io`, which is a *different*
// parachain (genesis 0xa22a2424…), not the one the shipping app uses. The
// Polkadot app's `paseo-next-v2` build (the nightly default) pairs over the
// People *system* chain `wss://paseo-people-next-system-rpc.polkadot.io`
// (genesis 0xc5af1826…) — per dotli-community `packages/config/src/network.ts`,
// the source of truth for this flow. Verified by querying chain_getBlockHash(0)
// against both endpoints. Flip the line below if pairing with the UNSTABLE build.
const STATEMENT_STORE_ENDPOINTS = ["wss://paseo-people-next-system-rpc.polkadot.io"]; // paseo-next-v2 (nightly)
// const STATEMENT_STORE_ENDPOINTS = ["wss://previewnet.substrate.dev/people"];      // UNSTABLE / previewnet build

// Surface host-papp's internal SSO/statement-store activity so we can see whether
// the socket connects, subscribes, and receives the phone's pairing response.
let debugWired = false;
function wireDebug() {
  if (debugWired) return;
  debugWired = true;
  try {
    onHostPappDebugMessage((e) => console.log("[hostpapp]", e?.layer, e?.event, e));
  } catch (err) {
    /* debug bus is best-effort */
  }
}

let adapter = null;

export function getAdapter() {
  if (!adapter) {
    wireDebug();
    console.log("[sso] connecting statement store:", STATEMENT_STORE_ENDPOINTS);
    // createPappAdapter has NO `endpoints` param — it defaults its statement-store
    // client to SS_STABLE_STAGE_ENDPOINTS (a dead node). So we build the lazy client
    // + statement-store adapter against the right endpoint ourselves and inject them
    // via `adapters` (the same approach product-sdk-terminal uses).
    const lazyClient = createLazyClient(
      // 120s, matching dotli-community's auth path — the default 40s is too
      // aggressive through tunnels, and POSITIVE_INFINITY never reconnects a
      // silently-dropped socket (the subscription would die unnoticed).
      getWsProvider(STATEMENT_STORE_ENDPOINTS, { heartbeatTimeout: 120_000 }),
    );
    const statementStore = createPapiStatementStoreAdapter(lazyClient);
    adapter = createPappAdapter({
      appId: APP_ID,
      // HandshakeMetadata fields are exact: hostName / hostVersion / hostIcon /
      // platformType / platformVersion (anything else is dropped → nameless device).
      hostMetadata: {
        hostName: "Unstation",
        hostVersion: "0.0.0",
        platformType: "desktop",
      },
      adapters: { lazyClient, statementStore },
    });
  }
  return adapter;
}

/** Does a paired session already exist? (Synchronous — may be `false` before the
 *  persisted session has hydrated from storage; use {@link awaitSession} at boot.) */
export function hasSession() {
  try {
    return getAdapter().sessions.sessions.read().length > 0;
  } catch (e) {
    return false;
  }
}

/**
 * Resolve to `true` once a paired session exists, or `false` after `timeoutMs`.
 *
 * host-papp hydrates the persisted session from storage **asynchronously** (its
 * session repo reads `localStorage` through a `ResultAsync`, so the in-memory
 * `sessions` list is empty for a microtask or two after `getAdapter()` returns).
 * A synchronous `hasSession()` at startup therefore races that hydration and
 * almost always loses — which made the app demand a fresh QR link on every
 * launch even though the pairing was saved. This subscribes and waits for the
 * real answer instead of guessing on an empty list.
 */
export function awaitSession(timeoutMs = 2500) {
  return new Promise((resolve) => {
    let mgr;
    try {
      mgr = getAdapter().sessions;
    } catch (e) {
      resolve(false);
      return;
    }
    if (mgr.read().length > 0) {
      resolve(true);
      return;
    }
    let done = false;
    let unsub = null;
    const finish = (v) => {
      if (done) return;
      done = true;
      try { unsub && unsub(); } catch (e) { /* ignore */ }
      resolve(v);
    };
    try {
      unsub = mgr.subscribe((sessions) => {
        if (sessions && sessions.length > 0) finish(true);
      });
    } catch (e) {
      resolve(false);
      return;
    }
    setTimeout(() => finish(false), timeoutMs);
  });
}

// ---------------------------------------------------------------------------
// Paired statement-store signer extraction
//
// At pairing, the phone grants this device's per-app slot account an on-chain
// statement-store *allowance* and host-papp persists that slot signing key
// encrypted in localStorage. host-papp only vends a `StatementProver` (a sign
// callback), not the raw key — but our mesh's chain writes happen in Rust, which
// needs the key itself. So we read + decrypt the stored slot key here, mirroring
// host-papp's exact scheme (AES-GCM, key = blake2b(appId,16), nonce =
// blake2b("nonce",32); plaintext = SCALE Vector<{productId:str, resource:Enum,
// slotAccountKey:Bytes}>), and hand it to Rust via `set_chain_identity`.
//
// NOTE: this depends on host-papp@0.8.10's internal storage format; revisit on
// upgrade. Returns a Uint8Array (the slot secret) or null if not present.
const strToBytes = (s) => new TextEncoder().encode(s);
function fromHex(h) {
  const s = h.startsWith("0x") ? h.slice(2) : h;
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.substr(i * 2, 2), 16);
  return out;
}
// Minimal SCALE compact-integer decode (modes 0–2 cover our sizes).
function scaleCompact(buf, pos) {
  const b0 = buf[pos], mode = b0 & 3;
  if (mode === 0) return [b0 >>> 2, pos + 1];
  if (mode === 1) return [((buf[pos] | (buf[pos + 1] << 8)) >>> 0) >>> 2, pos + 2];
  if (mode === 2) {
    const v = (buf[pos] + buf[pos + 1] * 256 + buf[pos + 2] * 65536 + buf[pos + 3] * 16777216);
    return [Math.floor(v / 4), pos + 4];
  }
  const len = (b0 >>> 2) + 4; let v = 0;
  for (let i = 0; i < len; i++) v += buf[pos + 1 + i] * Math.pow(256, i);
  return [v, pos + 1 + len];
}
function parseAllowances(bytes) {
  let pos = 0, n;
  [n, pos] = scaleCompact(bytes, pos);
  const out = [];
  for (let i = 0; i < n; i++) {
    let plen; [plen, pos] = scaleCompact(bytes, pos);
    const productId = new TextDecoder().decode(bytes.slice(pos, pos + plen)); pos += plen;
    const resTag = bytes[pos]; pos += 1; // Enum: 0=bulletin, 1=statementStore
    let klen; [klen, pos] = scaleCompact(bytes, pos);
    const slotAccountKey = bytes.slice(pos, pos + klen); pos += klen;
    out.push({ productId, resource: resTag === 1 ? "statementStore" : "bulletin", slotAccountKey });
  }
  return out;
}

/** The paired statement-store slot signing key (Uint8Array), or null. */
export function statementStoreSlotKey() {
  try {
    const sessions = getAdapter().sessions.sessions.read();
    if (!sessions || !sessions.length) return null;
    const sessionId = sessions[0].id;
    const lsKey = `polkadot_${APP_ID}_AllowanceKeys_${sessionId}`;
    const stored = (typeof localStorage !== "undefined") ? localStorage.getItem(lsKey) : null;
    if (!stored) { console.warn("[sso] no allowance blob at", lsKey); return null; }
    const aes = gcm(blake2b(strToBytes(APP_ID), { dkLen: 16 }), blake2b(strToBytes("nonce"), { dkLen: 32 }));
    const plain = aes.decrypt(fromHex(stored));
    const entry = parseAllowances(plain).find((e) => e.resource === "statementStore");
    if (!entry) { console.warn("[sso] no statementStore allowance entry yet"); return null; }
    console.log("[sso] statement-store slot key found:", entry.slotAccountKey.length, "bytes, product", entry.productId);
    return entry.slotAccountKey;
  } catch (e) {
    console.error("[sso] statementStoreSlotKey failed", e);
    return null;
  }
}

/**
 * Start pairing. Resolves `{ ok, session }` once linked, or `{ ok:false, error }`.
 *  - `onPairing(payload)` fires with the QR payload to display.
 *  - `onStatus(status)` fires on every `pairingStatus` transition (for UI + logs).
 *
 * Resolves on **whichever** completes first: the `pairingStatus` reaching
 * `'finished'`/`'pairingError'`, or `authenticate()`'s returned result. This is
 * robust to host-papp versions where the promise settles differently than the
 * status stream (the original bug: we only watched the promise and missed the
 * `'finished'` event, so a successful link never advanced the UI).
 */
export function signIn(onPairing, onStatus) {
  const a = getAdapter();
  return new Promise((resolve) => {
    let settled = false;
    let unsub;
    const cleanup = () => {
      try {
        typeof unsub === "function" ? unsub() : unsub?.unsubscribe?.();
      } catch (e) {
        /* ignore */
      }
    };
    const finish = (r) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(r);
    };

    try {
      unsub = a.sso.pairingStatus.subscribe((s) => {
        if (!s) return;
        if (typeof onStatus === "function") onStatus(s);
        if (s.step === "pairing" && typeof onPairing === "function") onPairing(s.payload);
        else if (s.step === "finished") finish({ ok: true, session: s.session ?? null });
        else if (s.step === "pairingError") finish({ ok: false, error: s.message || "pairing error" });
      });
    } catch (e) {
      /* subscription is best-effort; the promise path below still settles */
    }

    // Drive the flow and settle on its result too (whichever fires first wins).
    Promise.resolve()
      .then(() => a.sso.authenticate())
      .then((result) => {
        if (result && typeof result.match === "function") {
          result.match(
            (session) => finish({ ok: true, session: session ?? null }),
            (err) => finish({ ok: false, error: (err && err.message) || String(err) }),
          );
        } else {
          finish({ ok: true, session: result ?? null });
        }
      })
      .catch((e) => finish({ ok: false, error: (e && e.message) || String(e) }));
  });
}

export function abort() {
  try {
    getAdapter().sso.abortAuthentication?.();
  } catch (e) {
    /* ignore */
  }
}
