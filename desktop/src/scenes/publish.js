// Go Live (publisher) — name your stream, share a friendly link, watch it back.

import QRCode from 'qrcode';
import { invoke, NATIVE } from '../tauri.js';
import { S, go, refreshGoLiveBadge, netLabel } from '../state.js';
import { STRINGS } from '../copy.js';
import { makeInviteLink } from '../invite.js';
import { nativeUrl } from '../player.js';
import { updateSettingsStatus } from './settings.js';
import { ensureSignedIn } from './onboarding.js';

let pubViewersTimer = null;

// The Android shim sets this before main.js (and thus this module) evaluates.
const isMobile = () => window.__unstationPlatformType === 'mobile' || document.body.classList.contains('plat-android');

export function fmtDur(ms){ const s=Math.max(0,(ms/1000)|0), h=(s/3600)|0, m=((s%3600)/60)|0, ss=s%60; return (h>0?h+':'+String(m).padStart(2,'0'):String(m))+':'+String(ss).padStart(2,'0'); }
// Stat-cell bitrate: kbps ≥ 1000 reads as Mbps with one decimal; 0/unknown is "—".
export function fmtKbps(k){ k=k|0; if(k<=0) return '—'; return k>=1000 ? (k/1000).toFixed(1)+' Mbps' : k+' kbps'; }

/* ---- Go-Live preflight (publish-progress): identity → announced → encoder ---- */
// ok=false is a QUIET degradation (subdued amber + the detail once in the health
// note), never a blocking error — the stream itself keeps going.
const progressWarn = {};   // step → degradation detail, cleared when the step later succeeds
let encoderOk = false;
function setStep(k, cls){ const el=document.querySelector('#pubSteps .step[data-k="'+k+'"]'); if(el) el.className='step'+(cls?' '+cls:''); }
function resetPublishProgress(){ ['identity','announced','encoder'].forEach(k=>{ delete progressWarn[k]; setStep(k,''); }); encoderOk=false; }
export function applyPublishProgress(p){
  if(!p || !p.step) return;
  if(p.ok){
    delete progressWarn[p.step];
    setStep(p.step,'done');
    if(p.step==='encoder'){ encoderOk=true; cancelObsTimer(); }
  } else {
    setStep(p.step,'warn');
    if(p.detail) progressWarn[p.step]=p.detail;
    if(p.step==='encoder') encoderOk=false;
  }
  updatePubHealth();
}

/* ---- publish-stats: viewers + real bitrates, with quiet advisories ---- */
let lowIngestRuns = 0;     // consecutive stats updates with ingest below 300 kbps while live
let zeroViewersSince = 0;  // when the live viewer count last sat at 0 (wall clock, ms)
export function applyPublishStats(p){
  if(!p) return;
  S.lastViewers = p.viewers||0;
  S.ingestKbps = p.ingest_kbps||0;
  S.uplinkKbps = p.uplink_kbps||0;
  if(S.pubLive){
    lowIngestRuns = (S.ingestKbps < 300) ? lowIngestRuns+1 : 0;
    if(S.lastViewers > 0) zeroViewersSince = 0;
    else if(!zeroViewersSince) zeroViewersSince = Date.now();
  } else { lowIngestRuns = 0; zeroViewersSince = 0; }
  updatePubHealth();
}

// Streamer-facing Go Live health: live/waiting + viewers + uptime + bitrates + network.
// The note line shows ONE thing, in priority order: struggling encoder → nobody watching
// yet → a preflight degradation detail → the plain connected/waiting copy.
export function updatePubHealth(){
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  const dot=document.getElementById('phDot');
  const warnNote = progressWarn.encoder || progressWarn.announced || progressWarn.identity || '';
  if(S.pubLive){
    if(dot) dot.dataset.h='live'; set('phStatus','You’re live');
    let note = 'Your encoder is connected — viewers can tune in with your stream name.';
    if(lowIngestRuns >= 2) note = STRINGS.advEncoderStruggling;
    else if(zeroViewersSince && Date.now()-zeroViewersSince > 120000) note = STRINGS.advNoViewers;
    else if(warnNote) note = warnNote;
    set('phNote', note);
  }
  else { if(dot) dot.dataset.h='wait'; set('phStatus','Waiting for your encoder'); set('phNote', warnNote || 'Point OBS at the server above and start streaming — it goes live on its own.'); }
  set('phViewers', String(S.lastViewers));
  set('phUptime', S.pubLiveSince ? fmtDur(Date.now()-S.pubLiveSince) : '—');
  set('phNet', netLabel().t);
  set('phIngest', fmtKbps(S.ingestKbps));
  set('phUplink', fmtKbps(S.uplinkKbps));
}
// Live uptime ticker (cheap; only repaints while a stream is actually live).
setInterval(()=>{ if(S.publishing && S.pubLive) updatePubHealth(); }, 1000);

/* ---- OBS guided setup: auto-expand if the encoder never connects ---- */
// Armed on entering the live console; opens the panel if `encoder` hasn't gone ok
// within 60s. Cancelled when the encoder connects or the console is left.
let obsTimer = null;
function armObsTimer(){
  cancelObsTimer();
  if(isMobile() || encoderOk) return; // the RTMP/OBS card is desktop-only
  obsTimer = setTimeout(()=>{ obsTimer=null;
    const d=document.getElementById('obsSetup');
    if(d && !encoderOk && S.publishing && !document.getElementById('pubLive').hidden) d.open=true;
  }, 60000);
}
function cancelObsTimer(){ if(obsTimer){ clearTimeout(obsTimer); obsTimer=null; } }

/* ---- one-time console copy from copy.js (labels differ by platform) ---- */
{
  const lbl = { identity: STRINGS.stepIdentity, announced: STRINGS.stepAnnounced, encoder: isMobile() ? STRINGS.stepCamera : STRINGS.stepEncoder };
  document.querySelectorAll('#pubSteps .step').forEach(s=>{ const t=s.querySelector('.s-lbl'); if(t) t.textContent = lbl[s.dataset.k] || s.dataset.k; });
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  set('phIngestK', isMobile() ? STRINGS.stepCamera : STRINGS.statEncoder);
  set('phUplinkK', STRINGS.statUplink);
  set('inviteHint', STRINGS.inviteHint);
  set('showQrBtn', STRINGS.showQr);
  set('shareInviteBtn', STRINGS.shareSheet);
  set('inviteQrClose', STRINGS.close);
  const sh=document.getElementById('shareInviteBtn'); if(sh) sh.hidden = !isMobile();
}

/* ---- ingest: no user-facing protocol choice ----
   The default is the modern encoder path (WHIP: OBS 30+, lower latency, enables fast
   connect); a quiet link in the console switches to the classic RTMP setup for older
   encoders — offered only while WAITING for the encoder, never under a live stream.
   Android publishes from the camera; none of this applies there. */
let ingestMode = 'whip';

// Populate the console's ingest card for the mode the backend actually opened
// (info.ingest_mode): WHIP shows a single URL (no key) + OBS-30 steps; RTMP shows
// Server + Stream key + classic steps. Used by go-live, tab-back re-attach, and the
// setup-switch link.
function renderIngestCard(info){
  const whip = info.ingest_mode === 'whip';
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  set('ingestServer', info.ingest_server);
  set('ingestKey', info.stream_key);
  set('ingestServerK', whip ? 'URL' : 'Server');
  const kf=document.getElementById('ingestKeyField'); if(kf) kf.style.display = whip ? 'none' : '';
  set('obsSetupSummary', whip ? STRINGS.obsSetupTitleWhip : STRINGS.obsSetupTitle);
  const hint=document.getElementById('pubWaitingHint');
  if(hint && !isMobile()) hint.textContent = whip ? STRINGS.whipWaitingHint : STRINGS.rtmpWaitingHint;
  const ol=document.getElementById('obsSteps');
  if(ol){ ol.innerHTML='';
    (whip ? [STRINGS.whipStep1, STRINGS.whipStep2, STRINGS.whipStep3]
          : [STRINGS.obsStep1, STRINGS.obsStep2, STRINGS.obsStep3])
      .forEach(t=>{ const li=document.createElement('li'); li.textContent=t; ol.appendChild(li); });
  }
  const sw=document.getElementById('ingestSwitch');
  if(sw){
    sw.textContent = whip ? STRINGS.ingestSwitchToClassic : STRINGS.ingestSwitchToModern;
    sw.hidden = isMobile() || S.pubLive; // never offer a restart under a live stream
  }
  // Fast-connect invites exist only where the fast tier does (the WHIP publish path).
  const fc=document.getElementById('fastInviteChip');
  if(fc) fc.hidden = !whip;
  if(!whip && fastInviteOn){ fastInviteOn = false; refreshInviteUi(); }
}

// Switch encoder setups while waiting (stop + restart the publish in the other mode).
async function switchIngest(){
  if(S.pubLive || !NATIVE || !invoke) return;
  ingestMode = ingestMode === 'whip' ? 'rtmp' : 'whip';
  try{
    await invoke('stop_publish');
    const info = await invoke('start_publish', { title: S.pubName, ingestMode });
    S.pubHlsUrl = info.hls_url;
    renderIngestCard(info);
  }catch(err){ publishStartFailed(err); }
}
{ const sw=document.getElementById('ingestSwitch'); if(sw) sw.addEventListener('click', switchIngest); }

/* ---- invite link + QR + share sheet ----
   The invite row can flip into a FAST-CONNECT invite (a ?fast link): the broadcaster's
   explicit act of trust — direct, sooner, unverified video for a few friends. */
let fastInviteOn = false;
function refreshInviteUi(){
  const link = S.pubName ? makeInviteLink(S.pubName, fastInviteOn) : '';
  const el=document.getElementById('inviteLink'); if(el) el.textContent = link || '—';
  const fc=document.getElementById('fastInviteChip');
  if(fc){ fc.classList.toggle('on', fastInviteOn); fc.setAttribute('aria-pressed', fastInviteOn?'true':'false'); }
  const hint=document.getElementById('fastInviteHint');
  if(hint) hint.hidden = !fastInviteOn;
  return link;
}
{ const fc=document.getElementById('fastInviteChip');
  if(fc){ fc.textContent = STRINGS.fastInviteChip; fc.addEventListener('click', ()=>{ fastInviteOn = !fastInviteOn; refreshInviteUi(); }); } }
{ const h=document.getElementById('fastInviteHint'); if(h) h.textContent = STRINGS.fastInviteOnHint; }
async function showInviteQr(){
  const link=refreshInviteUi(); if(!link) return;
  const box=document.getElementById('inviteQrBox'); if(!box) return;
  try {
    const svg = await QRCode.toString(link, {
      type: 'svg', errorCorrectionLevel: 'M', margin: 1,
      color: { dark: '#0B0B0E', light: '#0000' },
    });
    document.getElementById('inviteQrSvg').innerHTML = svg;
  } catch(e){ console.error('invite qr render failed', e); return; }
  const cl=document.getElementById('inviteQrLink'); if(cl) cl.textContent=link;
  box.hidden=false;
}
function hideInviteQr(){ const box=document.getElementById('inviteQrBox'); if(box) box.hidden=true; }
{ const b=document.getElementById('showQrBtn'); if(b) b.addEventListener('click', showInviteQr); }
{ const box=document.getElementById('inviteQrBox');
  if(box) box.addEventListener('click', e=>{ const t=e.target; if(t===box || (t.closest && t.closest('#inviteQrClose'))) hideInviteQr(); }); }
{ const b=document.getElementById('shareInviteBtn');
  if(b) b.addEventListener('click', async ()=>{
    const link=refreshInviteUi(); if(!link) return;
    if(navigator.share){ try{ await navigator.share({ url: link, title: S.pubName }); return; }catch(e){ if(e && e.name==='AbortError') return; } }
    try{ await navigator.clipboard.writeText(link); }catch(e){}
    const old=b.textContent; b.textContent=STRINGS.copied; b.classList.add('done');
    setTimeout(()=>{ b.textContent=old; b.classList.remove('done'); }, 1200);
  }); }

/* ---- camera-permission recovery (mobile): Open Settings / Try again ---- */
function showCamRecovery(show){
  const w=document.getElementById('pubWaiting'); if(!w) return;
  let row=document.getElementById('camRecovery');
  if(!row){
    if(!show) return;
    row=document.createElement('div'); row.id='camRecovery'; row.className='cam-recovery';
    row.innerHTML='<button class="btn" id="camSettingsBtn" type="button"></button><button class="btn ghost" id="camRetryBtn" type="button"></button>';
    w.appendChild(row);
    row.querySelector('#camSettingsBtn').textContent=STRINGS.openSettings;
    row.querySelector('#camRetryBtn').textContent=STRINGS.tryAgain;
  }
  row.style.display = show ? 'flex' : 'none';
}
// A publish-start failure lands in the waiting box; camera-permission rejections
// additionally get the Open Settings / Try again actions.
function publishStartFailed(err){
  const msg=''+((err && err.message) || err);
  const b=document.querySelector('#pubWaiting b');
  if(/camera/i.test(msg)){ if(b) b.textContent=STRINGS.camPermHelp; showCamRecovery(true); }
  else if(b) b.textContent=msg;
}
// The recovery row is injected HTML — delegate its clicks.
{ const w=document.getElementById('pubWaiting');
  if(w) w.addEventListener('click', async e=>{
    const b=e.target && e.target.closest ? e.target.closest('button') : null; if(!b) return;
    if(b.id==='camSettingsBtn'){ if(NATIVE && invoke) invoke('open_app_settings').catch(()=>{}); }
    else if(b.id==='camRetryBtn'){
      showCamRecovery(false);
      const bEl=document.querySelector('#pubWaiting b');
      if(bEl) bEl.textContent = isMobile() ? STRINGS.camStarting : 'Waiting for your encoder…';
      try{ if(window.__onPublishStarted) await window.__onPublishStarted(); }catch(err){ publishStartFailed(err); }
    }
  }); }

export const slugify = t => ((t||'').trim().toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/^-+|-+$/g,'') || 'my-stream');
// The stream's shareable name — just the text a viewer types. No ".dot": chain-
// backed `.dot` resolution lands later, and the Rust side hashes the same
// canonical name whether or not a ".dot" is present.
export const shareName = t => slugify(t);
const titleEl = document.getElementById('streamTitle');
const startStreamBtn = document.getElementById('startStream');
titleEl.addEventListener('input', ()=>{ document.getElementById('sharePreview').textContent = shareName(titleEl.value); });

// Step 1 — name the stream. We deliberately do NOT open the ingest yet: the
// stream's identity is derived from this name, so it must be fixed before we
// publish presence. (Opening on entry with an empty box made every stream
// "my-stream" and broke discovery.)
export function showPubSetup(){
  document.getElementById('pubSetup').hidden = false;
  document.getElementById('pubLive').hidden = true;
  document.getElementById('pubHeadline').textContent = 'Name your stream';
  titleEl.readOnly = false;
  startStreamBtn.hidden = false;
  document.getElementById('sharePreview').textContent = shareName(titleEl.value);
}

// Step 2 — lock the name in, open the ingest, and enter the console. From here the
// ingest stays available (the Rust feeder keeps the listener up), so the encoder
// can connect ANY time, in any order — `publish-state` flips the console
// LIVE/waiting to match the actual video.
export async function goLiveStart(){
  if(!S.publishing) return;
  if(!ensureSignedIn()) return;
  S.pubName = shareName(titleEl.value);
  titleEl.value = S.pubName;
  titleEl.readOnly = true;
  document.getElementById('shareLink').textContent = S.pubName;
  refreshInviteUi();
  document.getElementById('pubViewers').textContent = '0';
  document.getElementById('pubSetup').hidden = true;
  document.getElementById('pubLive').hidden = false;
  resetPublishProgress(); showCamRecovery(false);
  armObsTimer();
  applyPublishState(false); // enter the console in the waiting state
  if(NATIVE && invoke){
    try {
      const info = await invoke('start_publish', { title: S.pubName, ingestMode });
      S.pubHlsUrl = info.hls_url; S.pubActive = true; refreshGoLiveBadge();
      renderIngestCard(info);
      // Android: start_publish has opened the AU intake; now start the camera capture
      // (a no-op seam on desktop, which ingests via RTMP/OBS instead). Its error — e.g. a
      // camera-permission rejection — surfaces in the same waiting UI (with Open Settings /
      // Try again actions when it's a camera-permission problem).
      if(window.__onPublishStarted) await window.__onPublishStarted();
    } catch(err){ publishStartFailed(err); }
  } else {
    S.pubActive = true; refreshGoLiveBadge();
    // preview demo: walk the preflight, then flip live
    setTimeout(()=>{ if(S.publishing) applyPublishProgress({step:'identity',ok:true,detail:''}); }, 500);
    setTimeout(()=>{ if(S.publishing) applyPublishProgress({step:'announced',ok:true,detail:''}); }, 1100);
    setTimeout(()=>{ if(S.publishing){ applyPublishProgress({step:'encoder',ok:true,detail:''}); applyPublishState(true); } }, 1800);
  }
}

// Reflect the REAL stream state — `publish-state` fires from the feeder based on
// whether fresh fragments are actually arriving, so LIVE always matches the video.
export function applyPublishState(live){
  S.pubLive = live;
  if(live){
    if(!S.pubLiveSince) S.pubLiveSince = Date.now();
    if(!zeroViewersSince) zeroViewersSince = Date.now(); // arm the "no one's connected yet" clock
    cancelObsTimer();
    if(window.__keepAwake) window.__keepAwake(true); // a background publish still keeps the device awake
  } else { S.pubLiveSince = 0; lowIngestRuns = 0; zeroViewersSince = 0; }
  // The setup-switch link restarts the ingest — never offer that under a live stream.
  { const sw=document.getElementById('ingestSwitch'); if(sw) sw.hidden = isMobile() || live; }
  updatePubHealth();
  if(S.curState==='settings') updateSettingsStatus();
  // Only drive the on-screen console video when it's visible — a background publish
  // still emits publish-state while the user is on another tab.
  if(!S.publishing || document.getElementById('pubLive').hidden) return;
  const tag = document.getElementById('pubLiveTag');
  const waiting = document.getElementById('pubWaiting');
  const v = document.getElementById('pubVid');
  if(tag) tag.style.display = live ? '' : 'none';
  if(waiting) waiting.style.display = live ? 'none' : 'flex';
  document.getElementById('pubHeadline').textContent = live ? 'You’re live.' : (window.__unstationPlatformType === 'mobile' ? 'Starting your camera…' : 'Waiting for your encoder…');
  if(live){
    if(NATIVE && S.pubHlsUrl){
      // Android's Chromium WebView has no native HLS — play the self-preview through hls.js
      // (the same seam the watch path uses). Desktop (WKWebView) plays natively — via the
      // parts-free /std.m3u8 view (AVPlayer rejects the LL playlist; see player.js nativeUrl).
      if(window.__hlsPlay){ window.__hlsPlay(v, S.pubHlsUrl, null); }
      else { try{ if(v.canPlayType('application/vnd.apple.mpegurl')){ const u=nativeUrl(S.pubHlsUrl); v.src=u + (u.includes('?')?'&':'?') + 't=' + Date.now(); v.style.display='block'; v.load(); v.play().catch(()=>{}); } }catch(e){} }
    }
  } else {
    try{ v.pause(); }catch(e){} v.removeAttribute('src'); v.style.display='none';
  }
}

// Go Live tab — re-attach to a running stream (tab-back) instead of restarting it,
// or open the name-entry step if nothing is publishing yet.
export async function enterGoLive(){
  if(!ensureSignedIn()) return;
  go('publish');
  let status = null;
  if(NATIVE && invoke){ try{ status = await invoke('publish_status'); }catch(e){} }
  if(status){
    S.publishing = true; S.pubActive = true; S.pubName = status.name; S.pubHlsUrl = status.info.hls_url;
    titleEl.value = status.name; titleEl.readOnly = true;
    renderIngestCard(status.info);
    document.getElementById('shareLink').textContent = status.name;
    refreshInviteUi();
    S.lastViewers = status.viewers||0;
    document.getElementById('pubViewers').textContent = S.lastViewers;
    document.getElementById('pubSetup').hidden = true;
    document.getElementById('pubLive').hidden = false;
    refreshGoLiveBadge();
    if(!status.live) armObsTimer();
    // Re-attach (e.g. a webview reload) gets no replayed progress events — synthesize
    // what the session's existence proves: identity booted; encoder ok iff live now.
    applyPublishProgress({ step:'identity', ok:true, detail:'' });
    if(status.live) applyPublishProgress({ step:'encoder', ok:true, detail:'' });
    applyPublishState(!!status.live);
  } else {
    S.publishing = true;
    showPubSetup();
    titleEl.focus(); if(titleEl.select) titleEl.select();
  }
}

export function endPublish(){
  S.publishing = false; S.pubActive = false; S.pubName = ''; S.pubLive = false; S.pubLiveSince = 0; S.lastViewers = 0; S.ingestKbps = 0; S.uplinkKbps = 0; clearInterval(pubViewersTimer);
  cancelObsTimer(); resetPublishProgress(); showCamRecovery(false); hideInviteQr();
  lowIngestRuns = 0; zeroViewersSince = 0;
  if(window.__keepAwake) window.__keepAwake(false);
  refreshGoLiveBadge();
  // Update the UI immediately — never block the button on the backend teardown. The camera
  // stop (MediaCodec + Camera2 close) can take a moment on mobile; awaiting it here made
  // "End stream" feel dead. Tear the backend + camera down in the background instead.
  titleEl.readOnly = false;
  document.getElementById('pubLive').hidden = true;
  const tag=document.getElementById('pubLiveTag'); if(tag) tag.style.display='none';
  const v=document.getElementById('pubVid'); try{ v.pause(); }catch(e){} v.removeAttribute('src'); v.style.display='none';
  go('entry');
  if(NATIVE && invoke){ invoke('stop_publish').catch(()=>{}); }
  if(window.__onPublishStopped){ Promise.resolve(window.__onPublishStopped()).catch(()=>{}); }
}
