// Onboarding: pairing with the Polkadot app (QR / same-device deep-link) and bridging
// the paired statement-store allowance — the "network pass" — to the Rust backend.

import QRCode from 'qrcode';
import * as sso from '../sso.js';
import { invoke, NATIVE } from '../tauri.js';
import { S, go } from '../state.js';
import { humanizeError } from '../copy.js';
import { startWatch } from './watch.js';

// After a successful sign-in, resume what the user came here to do: a deep-linked
// invite (S.pendingWatch, stashed by main.js's invite handler) goes straight to the
// stream; otherwise land on entry. Every sign-in success path funnels through this.
// Multiple success callbacks can race onto one coalesced pushChainIdentity — the first
// consumes the stash; later ones must not yank an already-started watch back to entry.
export function resumeAfterSignIn(){
  const target = S.pendingWatch; S.pendingWatch = '';
  const key = S.pendingWatchKey; S.pendingWatchKey = undefined;
  if(target){ startWatch(target, key); return; }
  if(['finding','live','seed','catchup'].includes(S.curState)) return;
  go('entry');
}

/* QR — real, scannable (the `qrcode` lib). Encodes the SSO pairing payload;
   until host-papp provides the real payload (SSO-2) this renders a placeholder URI. */
export async function renderQr(payload){
  try {
    const svg = await QRCode.toString(payload || 'polkadot://unstation/pair?v=1', {
      type: 'svg', errorCorrectionLevel: 'M', margin: 1,
      color: { dark: '#0B0B0E', light: '#0000' },
    });
    document.getElementById('qrBox').innerHTML = svg;
  } catch(e){ console.error('qr render failed', e); }
}
window.__renderPairingQr = renderQr;   // SSO-2 calls this with the live pairing payload
renderQr();

// Real Polkadot-app pairing (native only): show the live pairing QR, advance on approval.
// Shared onboarding status line + the no-reset retry affordance (revealed on a failed
// allowance so the user can retry WITHOUT resetPairing, which would wipe the cached
// grant and burn another statement-store slot).
export function onboardingStatus(txt, show=true){
  const el=document.getElementById('pairStatus'), t=document.getElementById('pairStatusText');
  if(el) el.style.display = show ? 'flex' : 'none';
  if(t) t.textContent = txt;
}
export function showRetry(show){ const r=document.getElementById('allowRetry'); if(r) r.style.display = show ? 'flex' : 'none'; }

// Sign-in is required to watch or publish — every mesh write (presence, signaling,
// edge) needs the paired statement-store allowance. Soft-gate the entry points:
// bounce to pairing when there's no live session yet. (Sync hasSession() is reliable
// here — boot awaits hydration before any of these controls are reachable.)
export function ensureSignedIn(){
  if(!NATIVE) return true;
  if(S.chainReady) return true;
  // A pairing session may exist without a usable chain identity (allowance not yet
  // bridged, or a stale session). Reuse the session and retry the allowance WITHOUT
  // resetPairing (no slot churn); only fall back to a fresh pair if there's no session.
  go('onboarding');
  const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display='none';
  let have=false; try{ have=sso.hasSession(); }catch(e){}
  if(have){ onboardingStatus('Finishing network access on your phone…'); pushChainIdentity().then(ok=>{ if(ok){ onboardingStatus('Network access granted ✓'); setTimeout(resumeAfterSignIn, 700); } }); }
  else beginPairing();
  return false;
}

export async function beginPairing(){
  const explain = document.querySelector('[data-scene="onboarding"] .qr-copy p');
  const setStatus = onboardingStatus;
  showRetry(false);
  try {
    const res = await sso.signIn(
      payload => { renderQr(payload); if (window.__onPairingPayload) window.__onPairingPayload(payload); },
      s => {
        console.log('[sso] pairingStatus:', s.step, s);
        if(s.step==='pairing') setStatus('Waiting for you to scan…');
        else if(s.step==='pending') setStatus('Finishing sign-in' + (s.stage ? (' · ' + s.stage) : '') + '…');
        else if(s.step==='finished') setStatus('Linked ✓');
        else if(s.step==='pairingError') setStatus('Error: ' + (s.message || 'pairing failed'));
      }
    );
    console.log('[sso] signIn result:', res);
    if(res && res.ok){
      setStatus('Linked ✓ — one more step on your phone');
      // Let the phone leave its pairing screen before we request the allowance:
      // firing it instantly races the pairing-complete transition and the
      // approval prompt flashes away before it can be acted on.
      await new Promise(r=>setTimeout(r, 1600));
      const ok = await pushChainIdentity(setStatus);
      if(ok){ setStatus('Network access granted ✓'); setTimeout(resumeAfterSignIn, 800); }
      // On failure pushChainIdentity left an explanatory status; stay on the
      // onboarding screen so the QR + "I've scanned it" remain for a retry.
    }
    else if(explain){ explain.textContent = 'Sign-in didn’t finish' + (res && res.error ? (' (' + res.error + ')') : '') + '. You can Skip for now and try again later.'; }
  } catch(e){ console.error('[sso] beginPairing threw', e); setStatus('Error: ' + ((e && e.message) || e)); }
}

// Bridge the paired statement-store allowance to the Rust signer: extract the
// slot key (decrypted from host-papp's storage) and hand it to the backend so all
// mesh chain writes are allowance-backed. Without this, presence/signaling are
// rejected on-chain and nothing connects. Safe to call repeatedly (Rust is idempotent).
let _identityInFlight = null; // single-flight guard so overlapping callers don't fire concurrent allocations
export async function pushChainIdentity(onStatus = onboardingStatus){
  if(!(NATIVE && invoke)) return false;
  // Single-flight: boot + ensureSignedIn + the Try-again button can all call this.
  // Firing concurrent ResourceAllocationRequests on one session makes the phone dispose
  // it ("Session disposed"), so coalesce overlapping calls onto one in-flight promise.
  if(_identityInFlight) return _identityInFlight;
  const say = (t)=>{ try{ onStatus && onStatus(t); }catch(e){} };
  _identityInFlight = (async () => {
  try {
    // Provision the statement-store allowance from the phone FIRST. On the initial
    // pairing this prompts for approval on the device; on later launches it's a
    // cached no-op. Only after a grant does the slot-key blob exist to decrypt.
    say('Open your Polkadot app and approve network access — you may need to sign a transaction. This can take a minute; keep the app open.');
    const granted = await sso.requestStatementStoreAllowance();
    if(!granted){
      const reason = (sso.getLastAllowanceError && sso.getLastAllowanceError()) || '';
      console.warn('[chain] statement-store allowance not granted —', reason || 'unknown');
      S.chainReady = false;
      say(humanizeError(reason));
      showRetry(true);
      return false;
    }
    const key = sso.statementStoreSlotKey();
    if(!key){ console.warn('[chain] allowance granted but slot-key blob missing'); say('Network access granted, but the key didn’t load. Tap Try again.'); showRetry(true); return false; }
    await invoke('set_chain_identity', { slotSecret: Array.from(key) });
    console.log('[chain] paired identity sent to backend');
    // Best-effort: also grab the Bulletin allowance so durable-origin manifest writes
    // are sponsored. Non-fatal — the live stream + mesh work without it; this only
    // restores the on-chain cold-start / late-joiner anchor.
    try {
      if(await sso.requestBulletinAllowance()){
        const bkey = sso.bulletinSlotKey();
        if(bkey){ await invoke('set_bulletin_identity', { slotSecret: Array.from(bkey) }); S.bulletinReady = true; console.log('[chain] bulletin allowance sent to backend'); }
      }
    } catch(e){ console.warn('[chain] bulletin allowance setup skipped', e); }
    S.chainReady = true; showRetry(false);
    return true;
  } catch(e){ console.error('[chain] set_chain_identity failed', e); S.chainReady = false; showRetry(true); return false; }
  })();
  try { return await _identityInFlight; } finally { _identityInFlight = null; }
}
