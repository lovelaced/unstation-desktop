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
      appId: "unstation",
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

/** Does a paired session already exist? */
export function hasSession() {
  try {
    return getAdapter().sessions.sessions.read().length > 0;
  } catch (e) {
    return false;
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
