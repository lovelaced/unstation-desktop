// Local media plumbing: attaching <video> elements to the local HLS server, plus the
// viewer-facing catchup overlay + watchdog that turn a silent "no video" into words.

import { S } from './state.js';
import { STRINGS } from './copy.js';

let watchWatchdog = null;

export function isVideoPlaying(id){ const v=document.getElementById(id); return !!(v && v.readyState>=3 && !v.paused && !v.ended); }

// Viewer-facing connection feedback. The catchup overlay carries the message; the
// watchdog turns a silent "no video" into a peer-aware, actionable explanation.
export function setCatchup(html){ const c=document.getElementById('catchup'); if(c){ c.innerHTML=html; c.style.display='grid'; } }
export function clearWatchUi(){ clearTimeout(watchWatchdog); const c=document.getElementById('catchup'); if(c){ c.style.display='none'; } }
export function startWatchWatchdog(){
  clearTimeout(watchWatchdog);
  setCatchup('<span class="spin"></span>'+STRINGS.watchConnecting);
  watchWatchdog = setTimeout(()=>{
    const v=document.getElementById('vid'); if(v.readyState>=3 && !v.paused) return; // already playing
    if(S.lastPeers>0) setCatchup(STRINGS.watchNoVideoFromPeer);
    else setCatchup(STRINGS.watchUnreachable);
  }, 18000);
}

// Attach the viewer to its local HLS server. The mesh delivers segments a moment
// AFTER we attach, so the first playlist read is empty → the player reports
// "error 4 / source unsupported". So we keep re-loading (cache-busted, to re-read
// the now-growing playlist) until media actually plays, showing "Catching up…"
// meanwhile. If it never plays, the peer count in the HUD tells the real story.
export function setVideo(url){
  const v=document.getElementById('vid'), catchup=document.getElementById('catchup'); if(!url)return;
  if(window.__hlsPlay){ window.__hlsPlay(v, url, catchup); return; }  // Android: play via hls.js (no native HLS)
  if(!(v.canPlayType('application/vnd.apple.mpegurl'))){ if(catchup){ catchup.textContent=STRINGS.formatUnsupported; catchup.style.display='grid'; } return; }
  const attempt=()=>{ if(v.readyState>=3 && !v.paused) return; try{ v.src=url+(url.includes('?')?'&':'?')+'t='+Date.now(); v.style.display='block'; v.load(); v.play().catch(()=>{}); }catch(e){} };
  attempt();
  clearInterval(v._retry);
  v._retry=setInterval(()=>{ const playing=v.readyState>=3 && !v.paused; if(catchup && S.curState!=='catchup') catchup.style.display=playing?'none':'grid'; if(playing){ clearInterval(v._retry); clearTimeout(watchWatchdog); } else attempt(); }, 1500);
}

// Surface real media ERRORS on-screen (the bundled DMG has no devtools). We only
// show genuine `error` events — `stalled`/`waiting` fire routinely at the live edge
// during normal HLS playback, so showing those would be a constant false alarm.
// `onScreen=false` (the viewer) logs to console only — a transient "error 4" is
// EXPECTED while the mesh spins up (handled by setVideo's retry + the catchup
// overlay), so flashing it on screen would just be alarming. The publisher's local
// ingest, by contrast, has no retry, so its errors are shown.
export function wireVideoDiag(vidId, diagId, onScreen){
  const el=document.getElementById(vidId), diag=document.getElementById(diagId);
  if(!el) return;
  const CODES={1:'aborted',2:'network/blocked',3:'decode',4:'src unsupported'};
  const hide=()=>{ if(diag) diag.hidden=true; };
  el.addEventListener('error', ()=>{ const e=el.error; const m='video error '+((e&&e.code)||'?')+' ('+((e&&CODES[e.code])||'?')+')'+(e&&e.message?': '+e.message:'')+' — '+(el.currentSrc||'no src'); console.error('[video]',vidId,m); if(onScreen && diag){ diag.textContent=m; diag.hidden=false; } });
  el.addEventListener('playing', hide);
  el.addEventListener('loadeddata', hide);
  el.addEventListener('timeupdate', hide);
}
