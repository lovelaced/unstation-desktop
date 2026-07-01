// Go Live (publisher) — name your stream, share a friendly link, watch it back.

import { invoke, NATIVE } from '../tauri.js';
import { S, go, refreshGoLiveBadge, netLabel } from '../state.js';
import { updateSettingsStatus } from './settings.js';
import { ensureSignedIn } from './onboarding.js';

let pubViewersTimer = null;

export function fmtDur(ms){ const s=Math.max(0,(ms/1000)|0), h=(s/3600)|0, m=((s%3600)/60)|0, ss=s%60; return (h>0?h+':'+String(m).padStart(2,'0'):String(m))+':'+String(ss).padStart(2,'0'); }

// Streamer-facing Go Live health: live/waiting + viewers + uptime + network. No jargon.
export function updatePubHealth(){
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  const dot=document.getElementById('phDot');
  if(S.pubLive){ if(dot) dot.dataset.h='live'; set('phStatus','You’re live'); set('phNote','Your encoder is connected — viewers can tune in with your stream name.'); }
  else { if(dot) dot.dataset.h='wait'; set('phStatus','Waiting for your encoder'); set('phNote','Point OBS at the server above and start streaming — it goes live on its own.'); }
  set('phViewers', String(S.lastViewers));
  set('phUptime', S.pubLiveSince ? fmtDur(Date.now()-S.pubLiveSince) : '—');
  set('phNet', netLabel().t);
}
// Live uptime ticker (cheap; only repaints while a stream is actually live).
setInterval(()=>{ if(S.publishing && S.pubLive) updatePubHealth(); }, 1000);

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
  document.getElementById('pubViewers').textContent = '0';
  document.getElementById('pubSetup').hidden = true;
  document.getElementById('pubLive').hidden = false;
  applyPublishState(false); // enter the console in the waiting state
  if(NATIVE && invoke){
    try {
      const info = await invoke('start_publish', { title: S.pubName });
      S.pubHlsUrl = info.hls_url; S.pubActive = true; refreshGoLiveBadge();
      document.getElementById('ingestServer').textContent = info.ingest_server;
      document.getElementById('ingestKey').textContent = info.stream_key;
      // Android: start_publish has opened the AU intake; now start the camera capture
      // (a no-op seam on desktop, which ingests via RTMP/OBS instead). Its error — e.g. a
      // camera-permission prompt — surfaces in the same waiting UI below.
      if(window.__onPublishStarted) await window.__onPublishStarted();
    } catch(err){ const w=document.getElementById('pubWaiting'); const b=w&&w.querySelector('b'); if(b) b.textContent=''+err; }
  } else {
    S.pubActive = true; refreshGoLiveBadge();
    setTimeout(()=>{ if(S.publishing) applyPublishState(true); }, 1800); // preview demo
  }
}

// Reflect the REAL stream state — `publish-state` fires from the feeder based on
// whether fresh fragments are actually arriving, so LIVE always matches the video.
export function applyPublishState(live){
  S.pubLive = live;
  if(live){ if(!S.pubLiveSince) S.pubLiveSince = Date.now(); } else { S.pubLiveSince = 0; }
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
      // (the same seam the watch path uses). Desktop (WKWebView) plays the m3u8 natively.
      if(window.__hlsPlay){ window.__hlsPlay(v, S.pubHlsUrl, null); }
      else { try{ if(v.canPlayType('application/vnd.apple.mpegurl')){ v.src=S.pubHlsUrl + (S.pubHlsUrl.includes('?')?'&':'?') + 't=' + Date.now(); v.style.display='block'; v.load(); v.play().catch(()=>{}); } }catch(e){} }
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
    document.getElementById('ingestServer').textContent = status.info.ingest_server;
    document.getElementById('ingestKey').textContent = status.info.stream_key;
    document.getElementById('shareLink').textContent = status.name;
    S.lastViewers = status.viewers||0;
    document.getElementById('pubViewers').textContent = S.lastViewers;
    document.getElementById('pubSetup').hidden = true;
    document.getElementById('pubLive').hidden = false;
    refreshGoLiveBadge();
    applyPublishState(!!status.live);
  } else {
    S.publishing = true;
    showPubSetup();
    titleEl.focus(); if(titleEl.select) titleEl.select();
  }
}

export function endPublish(){
  S.publishing = false; S.pubActive = false; S.pubName = ''; S.pubLive = false; S.pubLiveSince = 0; S.lastViewers = 0; clearInterval(pubViewersTimer);
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
