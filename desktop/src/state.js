// ANDROID SHIM CONTRACT
// ---------------------
// The Android repo single-sources this directory (its Vite root IS desktop/src):
// unstation-android/ui/android-shims.js is injected as an inline module BEFORE main.js
// and reaches directly into this code. Everything listed here must keep working
// exactly as-is — renaming or removing any of it breaks the Android build.
//
// window.* seams (neutral hooks; undefined on desktop, wired by the shim on Android):
//   window.__hlsPlay(v, url, catchup)  — defined by the shim; called by player.js setVideo()
//                                        and scenes/publish.js applyPublishState(). Android's
//                                        WebView has no native HLS, so playback goes through
//                                        hls.js/MSE.
//   window.__onPublishStarted()        — defined by the shim; awaited in scenes/publish.js
//                                        goLiveStart() (starts the phone camera capture).
//   window.__onPublishStopped()        — defined by the shim; called in scenes/publish.js
//                                        endPublish() (stops the camera in the background).
//   window.__onPairingPayload(payload) — defined by the shim; called from scenes/onboarding.js
//                                        beginPairing() with the live pairing payload (fires
//                                        the polkadotapp:// same-device deep-link).
//   window.__onSceneChange(state)      — defined by the shim; called at the top of go() on
//                                        every scene change. Drives the Android hardware-back
//                                        history (pushState/replaceState bookkeeping).
//   window.__go(state)                 — defined HERE (exposes go()); called by the shim's
//                                        popstate handler to route a hardware back press.
//   window.__hlsLatency()              — defined by the shim; live-edge lag in seconds from
//                                        hls.js (null when unknown). Used guardedly by
//                                        player.js's behind-live indicator.
//   window.__hlsSkipToLive()           — defined by the shim; seeks hls.js to the live edge.
//                                        Used guardedly by player.js's "Skip to live" chip.
//   window.__renderPairingQr(payload)  — defined by scenes/onboarding.js (renderQr); called by
//                                        SSO-2/host-papp with the live pairing payload.
//   window.__unstationPlatformType     — set to "mobile" by the shim; read by sso.js (pairing
//                                        handshake) and scenes/publish.js applyPublishState().
//   window.__keepAwake(on)             — defined by main.js on BOTH platforms (invokes the
//                                        set_keep_awake command; a desktop no-op). Called on
//                                        live watch/publish enter + leave/end.
//   window.__TAURI__                   — the Tauri bridge the shim invokes directly
//                                        (plugin:opener|open_url, camera_start, camera_stop).
//
// DOM the shim touches (do not rename/remove — see also index.html):
//   ids:       #pubWaiting (incl. its <b> and direct child <div>), #phStatus, #phNote,
//              #goLiveRec (moved onto the Go Live nav icon), #leaveWatchBtn (moved into
//              #net for a thumb-reachable Leave; also .click()ed by the back handler),
//              #net, #win (data-net), #inviteQrBox / #inviteQrClose (back handler)
//   selectors: [data-scene="onboarding"] .qr-copy p (onboarding copy rewrite),
//              .ingest-card, .pub-rail (+ its .eyebrow children) — hidden on mobile,
//              .titlebar .tab buttons (rebuilt as icon+label bottom-nav items).

import { initAmbient } from './ambient.js';
import { NATIVE } from './tauri.js';
import { renderViewerHealth } from './health.js';
import { STRINGS } from './copy.js';

export const win = document.getElementById('win');

// Ambient warm-glow field — see ./ambient.js. Wires the blur/visibility pause and
// returns the per-scene visibility toggle the state machine calls.
const setAmbient = initAmbient(win);

/* ---- shared mutable state ---- */
export const S = {
  curState: 'entry',
  publishing: false,
  pubActive: false,     // a publish session exists in the backend (live or waiting)
  pubName: '',
  pubKey: '',            // invite-only stream key (hex) for the current publish; embedded in the share link's #k=
  pubShield: false,      // "Hide my connection" (origin-shield) locked in at go-live: serve viewers via recruited relays only
  pubHlsUrl: null,
  pubLive: false,
  pubLiveSince: 0,
  lastViewers: 0,
  lastPeers: 0,
  chainState: '',
  chainDetail: '',
  // True once a usable, allowance-backed chain identity has been bridged to the backend
  // (set_chain_identity succeeded). A pairing *session* can exist without this, so gate
  // publish/watch and the "Signed in" status on chainReady, not on hasSession().
  chainReady: false,
  bulletinReady: false, // Bulletin allowance installed → durable-origin (manifest) writes sponsored
  fsOn: false,
  watchTarget: '',      // the stream name last submitted to start_watch — Rejoin/Try-again re-submit it
  watchKey: undefined,  // invite-only key from the watched link's #k= fragment (for decrypt + rejoin)
  pendingWatchKey: undefined, // invite key stashed with pendingWatch until sign-in completes
  pendingWatch: '',     // invite deep-link received before sign-in finished — resumed by resumeAfterSignIn
  fastFor: '',          // canonical stream name a fast-connect invite unlocked ('' = none)
  fastEligible: false,  // the CURRENT watch arrived via a fast-connect invite (set by startWatch)
  ingestKbps: 0,        // publish-stats: encoder → ingest bitrate (last 2s window)
  uplinkKbps: 0,        // publish-stats: mesh uplink to viewers (last 2s window)
};

/* ---- state machine ---- */
const scenes=[...document.querySelectorAll('.scene')], player=document.getElementById('player');
export const hud=document.getElementById('hud');
const titleCenter=document.getElementById('titleCenter'); let seqTimers=[], ttffTimer=null;
export function addSeqTimer(t){ seqTimers.push(t); }
export function setTtff(handle){ ttffTimer=handle; }
export function clearSeq(){ seqTimers.forEach(clearTimeout); seqTimers=[]; if(ttffTimer)cancelAnimationFrame(ttffTimer); }
export function showScene(name){ scenes.forEach(s=>s.classList.toggle('show', s.dataset.scene===name)); }
export function setTitle(stream,verified){ titleCenter.style.opacity=0; setTimeout(()=>{ if(verified){ titleCenter.innerHTML='<span class="verified"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 12l2 2 4-4"/><circle cx="12" cy="12" r="9"/></svg></span>'+stream; } else { titleCenter.textContent=stream; } titleCenter.style.opacity=1; }, 200); }

// Persistent top-level tabs. The current section is always highlighted; switching
// tabs never stops a running session (see enterGoLive/enterWatch + endPublish).
const tabsEl=document.getElementById('tabs'), tabEls=[...document.querySelectorAll('.tab')], goLiveRec=document.getElementById('goLiveRec');
export function tabForState(state){ if(state==='publish')return 'golive'; if(state==='settings')return 'settings'; if(state==='onboarding')return null; return 'watch'; }
export function setActiveTab(state){ if(tabsEl) tabsEl.hidden=(state==='onboarding'); const t=tabForState(state); tabEls.forEach(b=>b.classList.toggle('active', b.dataset.tab===t)); }
export function refreshGoLiveBadge(){ if(goLiveRec) goLiveRec.hidden=!S.pubActive; }

// scenes/watch.js registers runFinding() here at import time; go('finding') calls it
// through this hook so state.js never imports a scene module (no circular imports).
let findingHook=null;
export function registerFindingHook(fn){ findingHook=fn; }

export function go(state){ clearSeq(); S.curState=state;
  // Neutral scene-change hook (no-op on desktop): the Android shim keeps the SPA
  // history in step with the scene so the hardware back button behaves natively.
  if(window.__onSceneChange){ try{ window.__onSceneChange(state); }catch(e){} }
  document.querySelectorAll('.dock button').forEach(b=>b.classList.toggle('on', b.dataset.state===state));
  const isLive=['live','seed','catchup'].includes(state);
  setActiveTab(state);
  player.classList.toggle('show', isLive); hud.classList.toggle('show', isLive);
  document.getElementById('catchup').style.display = state==='catchup'?'grid':'none';
  // While stalled, the catchup spinner is the view's one animated element — and a
  // pulsing LIVE tag over frozen video would be dishonest anyway.
  { const lt=document.querySelector('#player .live-tag'); if(lt) lt.style.visibility = state==='catchup'?'hidden':''; }
  // Hold the screen awake only while video actually plays (live/seed) — NOT in
  // catchup/connecting. A watch stuck on "Connecting…" used to pin the screen on
  // indefinitely (keep-awake + active polling + cellular radio = the thermal-warning
  // combo measured on-device); if a stall outlasts the system screen timeout, dimming
  // is the correct behavior.
  if(isLive){ if(window.__keepAwake) window.__keepAwake(state!=='catchup'); const mode=state==='seed'?'seed':'p2p'; win.dataset.health=mode; setAmbient(false); document.getElementById('modeText').textContent=state==='seed'?STRINGS.modeLiveBackup:STRINGS.modeLiveP2p; showScene(''); if(!NATIVE){ setTitle('hardfork.dot',true); renderViewerHealth({peers: state==='seed'?6:23, playing:true, mode, publisher:'hardfork.dot'}); } return; }
  setAmbient(state==='entry'||state==='onboarding'||state==='ended'||state==='settings'); win.dataset.net='closed'; if(state!=='finding') setTitle('Unstation',false);
  if(state==='finding'){ showScene('finding'); if(findingHook) findingHook(); return; } showScene(state); }

// Expose the state machine as a neutral seam (see the contract header): the Android
// shim's back handler routes popstate through it. Unused on desktop.
window.__go = go;

// Plain network status shared by the Go Live card + Settings (driven by mesh-status).
export function netLabel(){
  if(S.chainState==='ready') return { t:'Connected', h:'good' };
  if(S.chainState==='connecting') return { t:'Connecting…', h:'wait' };
  if(S.chainState==='offline') return { t:'Not connected', h:'' };
  if(S.chainState==='error') return { t:(S.chainDetail||'Not connected'), h:'' };
  return { t: NATIVE?'Connecting…':'Preview', h:'wait' };
}
