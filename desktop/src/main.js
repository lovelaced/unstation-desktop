import './wasm-compat.js'; // must precede sso.js — neutralizes WKWebView's broken wasm streaming
import * as sso from './sso.js';
import { invoke, listen, NATIVE, appWindow } from './tauri.js';
import { renderViewerHealth } from './health.js';
import { S, go, win, hud, refreshGoLiveBadge } from './state.js';
import { wireVideoDiag } from './player.js';
import { applyStats, applyWatchPhase, enterWatch, leaveWatch, selfWatch, startWatch } from './scenes/watch.js';
import { enterGoLive, goLiveStart, endPublish, applyPublishState, updatePubHealth, applyPublishStats, applyPublishProgress } from './scenes/publish.js';
import { beginPairing, pushChainIdentity, onboardingStatus, showRetry, resumeAfterSignIn, ensureSignedIn } from './scenes/onboarding.js';
import { openSettings, updateSettingsStatus , applySeedStats } from './scenes/settings.js';
import { parseInviteLink } from './invite.js';

// G: keep the screen awake while something live is on screen (watch or publish).
// One neutral seam for both platforms — `set_keep_awake` is a no-op on desktop and
// FLAG_KEEP_SCREEN_ON on Android. Called from state.js (live entry), watch.js
// (leave/ended) and publish.js (live/end).
window.__keepAwake = (on)=>{ if(NATIVE && invoke) return invoke('set_keep_awake', { on: !!on }).catch(()=>{}); return Promise.resolve(); };

// E: unstation://watch/<name> invite links. If sign-in hasn't finished yet the name is
// stashed (S.pendingWatch) and the normal onboarding flow resumes it on success
// (resumeAfterSignIn); when already signed in it starts the watch immediately.
function handleInviteUrl(url){
  const name = parseInviteLink(url); if(!name) return;
  if(!NATIVE || S.chainReady){ startWatch(name); return; }
  S.pendingWatch = name;
  // Not signed in: surface the sign-in flow (a cached session coalesces onto the
  // in-flight allowance bridge). If onboarding/pairing is ALREADY on screen, leave it
  // be — its own success path resumes the stashed watch.
  if(S.curState!=='onboarding') ensureSignedIn();
}
// onOpenUrl implementations may replay getCurrent() — dedupe briefly so one open
// never starts the same watch twice, while a deliberate later re-open still works.
const seenInvites = new Set();
function onInviteUrls(urls){
  (urls||[]).forEach(u=>{
    if(!u || seenInvites.has(u)) return;
    seenInvites.add(u); setTimeout(()=>seenInvites.delete(u), 3000);
    handleInviteUrl(u);
  });
}

/* interactions */
function toggleNet(){ if(!hud.classList.contains('show'))return; const open=win.dataset.net==='open'; win.dataset.net=open?'closed':'open'; }

// Fullscreen for TV/laptop: hide the tabs + let the video fill the screen. Uses the
// Tauri window's native fullscreen on desktop; falls back to the HTML API in preview.
export async function toggleFullscreen(){
  S.fsOn=!S.fsOn; win.classList.toggle('fs', S.fsOn);
  const lbl=document.getElementById('fsLabel'); if(lbl) lbl.textContent=S.fsOn?'Exit':'Fullscreen';
  if(appWindow){ try{ await appWindow.setFullscreen(S.fsOn); }catch(e){ console.error('[fs]',e); } }
  else { try{ if(S.fsOn){ win.requestFullscreen && win.requestFullscreen(); } else if(document.fullscreenElement){ document.exitFullscreen(); } }catch(e){} }
}

document.getElementById('pill').addEventListener('click',toggleNet);
document.querySelectorAll('[data-goto]').forEach(b=>b.addEventListener('click',()=>go(b.dataset.goto)));

wireVideoDiag('pubVid','pubVidDiag', true);
wireVideoDiag('vid','vidDiag', false);

document.getElementById('pairedBtn').addEventListener('click', async ()=>{ if(NATIVE && invoke){ try{ await invoke('complete_signin'); }catch(e){} } go('entry'); });
document.getElementById('tabWatch').addEventListener('click', enterWatch);
document.getElementById('tabGoLive').addEventListener('click', enterGoLive);
document.getElementById('tabSettings').addEventListener('click', openSettings);
document.getElementById('goLiveLink').addEventListener('click', enterGoLive);
document.getElementById('leaveWatchBtn').addEventListener('click', leaveWatch);
document.getElementById('fsBtn').addEventListener('click', toggleFullscreen);
// Ended scene → Rejoin: re-submit the stored target through the same path as the
// watch form (a full start_watch). No target (e.g. preview dock) → back to entry.
{ const rj=document.getElementById('rejoinBtn'); if(rj) rj.addEventListener('click', ()=>{ if(S.watchTarget) startWatch(S.watchTarget); else go('entry'); }); }
{ const pv=document.getElementById('previewSelf'); if(pv) pv.addEventListener('click', ()=>{ if(S.pubName) selfWatch(S.pubName); }); }
{ const sb=document.getElementById('stopSeedBtn'); if(sb) sb.addEventListener('click', ()=>{ if(NATIVE && invoke) invoke('stop_seed').catch(()=>{}); sb.style.display='none'; const l=document.getElementById('setLend'); if(l) l.textContent='Off \u2014 not watching anything right now.'; }); }
  document.getElementById('rePairBtn').addEventListener('click', ()=>{ S.chainReady=false; try{ sso.resetPairing(); }catch(e){} const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display=''; go('onboarding'); beginPairing(); });
// Onboarding failure affordances: "Try again" re-requests the allowance on the EXISTING
// session (no resetPairing → reuses the cached grant, no slot churn); "Re-pair from
// scratch" is the nuclear option that wipes pairing state.
{ const rb=document.getElementById('retryAllowanceBtn'); if(rb) rb.addEventListener('click', async ()=>{ showRetry(false); onboardingStatus('Trying again…'); const ok=await pushChainIdentity(); if(ok){ onboardingStatus('Network access granted ✓'); setTimeout(resumeAfterSignIn, 700); } }); }
{ const rb2=document.getElementById('rePairBtn2'); if(rb2) rb2.addEventListener('click', ()=>{ S.chainReady=false; try{ sso.resetPairing(); }catch(e){} showRetry(false); const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display=''; beginPairing(); }); }
// Settings → "Grant access": (re)request the statement-store allowance on the existing
// session (triggers the phone popup if not yet granted; a no-op cache hit if it is).
{ const gb=document.getElementById('grantAccessBtn'); if(gb) gb.addEventListener('click', async ()=>{ gb.disabled=true; const prev=gb.textContent; gb.textContent='Requesting…'; try{ await pushChainIdentity(()=>{}); }catch(e){} gb.disabled=false; gb.textContent=prev; updateSettingsStatus(); }); }
document.getElementById('cancelPublish').addEventListener('click', endPublish);
document.getElementById('endStream').addEventListener('click', endPublish);
document.getElementById('startStream').addEventListener('click', goLiveStart);
document.getElementById('titleForm').addEventListener('submit', (e)=>{ e.preventDefault(); goLiveStart(); }); // Enter in the name field
document.querySelectorAll('.copy').forEach(btn => btn.addEventListener('click', async ()=>{
  const el = document.getElementById(btn.dataset.copy); const txt = el ? el.textContent : '';
  try { await navigator.clipboard.writeText(txt); } catch(e){}
  const old = btn.textContent; btn.textContent = 'Copied'; btn.classList.add('done');
  setTimeout(()=>{ btn.textContent = old; btn.classList.remove('done'); }, 1200);
}));

/* preview-only: dock + mock peer jitter */
if(!NATIVE){ const dock=document.getElementById('dock'); dock.style.display='flex'; document.querySelectorAll('.dock button').forEach(b=>b.addEventListener('click',()=>go(b.dataset.state)));
  setInterval(()=>{ if(!['live','seed','catchup'].includes(S.curState))return; const base=win.dataset.health==='seed'?6:23; const n=base+((Math.random()*5)|0)-2; renderViewerHealth({peers:n, playing:true, mode:win.dataset.health}); },2200);
} else { const d=document.getElementById('dock'); if(d) d.remove(); }

async function boot(){
  if(NATIVE){
    document.body.classList.add('native');
    let plat='macos'; try{ plat = await invoke('platform'); }catch(e){}
    document.body.classList.add('plat-'+plat);
    // Windows/Linux use a custom titlebar; wire its min/max/close to the native window.
    if(appWindow){ const wire=(id,fn)=>{ const b=document.getElementById(id); if(b) b.onclick=()=>{ try{ fn(); }catch(e){} }; }; wire('wcMin',()=>appWindow.minimize()); wire('wcMax',()=>appWindow.toggleMaximize()); wire('wcClose',()=>appWindow.close()); }
    if(listen){ try{
      await listen('mesh-stats', e=>applyStats(e.payload));
      await listen('watch-phase', e=>applyWatchPhase(e.payload));
      await listen('publish-state', e=>applyPublishState(!!(e.payload && e.payload.live)));
      await listen('publish-progress', e=>applyPublishProgress(e.payload));
      await listen('publish-stats', e=>{ if(e.payload){ applyPublishStats(e.payload); const el=document.getElementById('pubViewers'); if(el) el.textContent=S.lastViewers; if(S.curState==='settings') updateSettingsStatus(); } });
      await listen('publish-hint', e=>{ const w=document.getElementById('pubWaiting'); const b=w&&w.querySelector('b'); if(b && e.payload && e.payload.message){ b.textContent=e.payload.message; } });
      await listen('seed-stats', e=>applySeedStats(e.payload));
        await listen('mesh-status', e=>{ const p=e&&e.payload; if(!p) return; console.log('[mesh-status]', p.state, p.detail); S.chainState=p.state; S.chainDetail=p.detail||''; updatePubHealth(); if(S.curState==='settings') updateSettingsStatus(); if(p.state==='error'){ const b=document.querySelector('#pubWaiting b'); if(b && !document.getElementById('pubLive').hidden) b.textContent=p.detail; } });
    }catch(e){} }
    // Re-attach: if a publish session is still running in the backend (a webview
    // reload, or relaunch while the process lived), light the Go-Live tab badge so
    // the user can tab straight back into it.
    if(invoke){ try{ const ps = await invoke('publish_status'); if(ps){ S.pubActive = true; S.pubName = ps.name; S.pubHlsUrl = ps.info.hls_url; S.publishing = true; refreshGoLiveBadge(); } }catch(e){} }
    try {
      // Wait for the saved session to hydrate from storage (async) before
      // deciding — a sync check here races hydration and re-prompts for the QR.
      if(await sso.awaitSession()){
        go('entry');
        // Bridge the allowance to the backend. If it fails (often a stale session the
        // phone already disposed), surface onboarding with a retry instead of sitting
        // on entry looking signed-in while publish/watch silently fail. On success,
        // resume a deep-linked invite that arrived while the bridge was in flight.
        pushChainIdentity().then(ok=>{ if(!ok){ go('onboarding'); const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display='none'; showRetry(true); } else if(S.pendingWatch){ resumeAfterSignIn(); } });
      }
      else { go('onboarding'); document.getElementById('pairedBtn').style.display='none'; beginPairing(); }
    } catch(e){ go('entry'); }
    // Invite deep-links: runtime opens (onOpenUrl, which does NOT replay the startup
    // URL) + the URL that cold-started the app (getCurrent) — deduped in onInviteUrls.
    // Registered AFTER the session decision above so handleInviteUrl never races
    // hydration (a stashed invite is resumed by the sign-in success paths). The plugin
    // is registered in the shared Rust builder, so its API rides the injected
    // __TAURI__ global on both platforms; guard everything — an older shell without
    // the plugin just skips this.
    try {
      const dl = window.__TAURI__ && (window.__TAURI__.deepLink || window.__TAURI__['deep-link']);
      if(dl){
        if(dl.onOpenUrl) await dl.onOpenUrl(onInviteUrls);
        if(dl.getCurrent){ const urls = await dl.getCurrent(); if(urls) onInviteUrls(urls); }
      }
    } catch(e){ console.warn('[deep-link] unavailable', e); }
  } else { document.body.classList.add('plat-macos'); go('entry'); }
}
boot();
