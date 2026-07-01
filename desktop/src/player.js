// Local media plumbing: attaching <video> elements to the local HLS server, the
// viewer-facing catchup overlay + stall ladder that turn a silent "no video" into
// words, and the player controls (sound pill, mute toggle, behind-live/skip chip).

import { S } from './state.js';
import { STRINGS } from './copy.js';

export function isVideoPlaying(id){ const v=document.getElementById(id); return !!(v && v.readyState>=3 && !v.paused && !v.ended); }

/* ---- catchup overlay ---- */
// Viewer-facing connection feedback. The catchup overlay carries the message; the
// stall ladder below turns a silent "no video" into a peer-aware, actionable card.
export function setCatchup(html){ const c=document.getElementById('catchup'); if(c){ c.innerHTML=html; c.style.display='grid'; } }
export function hideCatchup(){ const c=document.getElementById('catchup'); if(c && S.curState!=='catchup') c.style.display='none'; }
export function clearWatchUi(){ cancelStallLadder(); const c=document.getElementById('catchup'); if(c){ c.style.display='none'; } hideSoundPill(); hideSkipChip(); soundHandled=false; }

/* ---- stall ladder (replaces the old single 18s watchdog) ----
   From watchdog start: 3s → a quiet "Catching up…" spinner; 20s → peer-aware honest
   copy (connected-but-no-video vs unreachable); 45s → a give-up card with "Try again"
   / "Leave" (wired by scenes/watch.js via delegation on #catchup — the buttons are
   injected HTML). Cancelled by any `playing` event, the `live` watch-phase, and every
   leave/ended teardown path (all of which run clearWatchUi → cancelStallLadder). */
let ladderTimers=[];
export function cancelStallLadder(){ ladderTimers.forEach(clearTimeout); ladderTimers=[]; }
function vidPlaying(){ const v=document.getElementById('vid'); return !!(v && v.readyState>=3 && !v.paused); }
export function startWatchWatchdog(){
  cancelStallLadder();
  ladderTimers.push(setTimeout(()=>{ if(!vidPlaying()) setCatchup('<span class="spin"></span>'+STRINGS.catchingUp); }, 3000));
  ladderTimers.push(setTimeout(()=>{ if(vidPlaying()) return;
    setCatchup(S.lastPeers>0 ? STRINGS.watchNoVideoFromPeer : STRINGS.watchUnreachable); }, 20000));
  ladderTimers.push(setTimeout(()=>{ if(vidPlaying()) return;
    const msg = S.lastPeers>0 ? STRINGS.watchNoVideoFromPeer : STRINGS.watchUnreachable;
    setCatchup('<div class="giveup"><div>'+msg+'</div><div class="giveup-row">'
      +'<button class="btn" id="retryWatchBtn" type="button">'+STRINGS.tryAgain+'</button>'
      +'<button class="btn ghost" id="giveUpLeaveBtn" type="button">'+STRINGS.leave+'</button></div></div>'); }, 45000));
}

// Attach the viewer to its local HLS server. The mesh delivers segments a moment
// AFTER we attach, so the first playlist read is empty → the player reports
// "error 4 / source unsupported". So we keep re-loading (cache-busted, to re-read
// the now-growing playlist) until media actually plays; the stall ladder narrates
// meanwhile. If it never plays, the peer count in the HUD tells the real story.
export function setVideo(url){
  const v=document.getElementById('vid'), catchup=document.getElementById('catchup'); if(!url)return;
  soundHandled=false; // a fresh attach = a fresh watch for the sound-pref logic
  // Re-assert the muted-autoplay invariant: a previous watch may have unmuted the
  // element. Attach muted (so play() can't be blocked); the saved sound preference
  // is restored on the first `playing` (handleSoundOnPlaying).
  try{ v.muted=true; }catch(e){}
  if(window.__hlsPlay){ window.__hlsPlay(v, url, catchup); return; }  // Android: play via hls.js (no native HLS)
  if(!(v.canPlayType('application/vnd.apple.mpegurl'))){ if(catchup){ catchup.textContent=STRINGS.formatUnsupported; catchup.style.display='grid'; } return; }
  const attempt=()=>{ if(v.readyState>=3 && !v.paused) return; try{ v.src=url+(url.includes('?')?'&':'?')+'t='+Date.now(); v.style.display='block'; v.load(); v.play().catch(()=>{}); }catch(e){} };
  attempt();
  clearInterval(v._retry);
  v._retry=setInterval(()=>{ const playing=v.readyState>=3 && !v.paused; if(playing){ if(catchup && S.curState!=='catchup') catchup.style.display='none'; clearInterval(v._retry); cancelStallLadder(); } else attempt(); }, 1500);
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

/* ---- sound: unmute pill + HUD mute toggle (persisted) ----
   The <video id="vid"> is hard-muted so autoplay never gets blocked. On the first
   `playing` of a watch we either restore the saved "unmuted" preference (falling back
   to the pill if the browser blocks a programmatic unmute) or show the pill. The
   publisher self-preview (#pubVid) stays permanently muted — none of this touches it. */
const SOUND_KEY='unstation_sound';
function soundPref(){ try{ const p=JSON.parse(localStorage.getItem(SOUND_KEY)||'null'); if(p && typeof p==='object') return { muted: p.muted!==false, volume: (typeof p.volume==='number' && p.volume>=0 && p.volume<=1) ? p.volume : 1 }; }catch(e){} return { muted:true, volume:1 }; }
function saveSoundPref(muted, volume){ try{ localStorage.setItem(SOUND_KEY, JSON.stringify({ muted:!!muted, volume:(typeof volume==='number'?volume:1) })); }catch(e){} }

const vid=document.getElementById('vid');
const soundPill=document.getElementById('soundPill');
const muteBtn=document.getElementById('muteBtn');
const skipBtn=document.getElementById('skipLiveBtn');
if(soundPill) soundPill.textContent=STRINGS.tapForSound;
if(skipBtn) skipBtn.textContent=STRINGS.skipToLive;

function showSoundPill(){ if(soundPill) soundPill.hidden=false; }
export function hideSoundPill(){ if(soundPill) soundPill.hidden=true; }
function hideSkipChip(){ if(skipBtn) skipBtn.hidden=true; }

const MUTED_SVG='<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H3v6h3l5 4V5Z"/><path d="m16 9 5 6M21 9l-5 6"/></svg>';
const UNMUTED_SVG='<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H3v6h3l5 4V5Z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M18.5 6a8.5 8.5 0 0 1 0 12"/></svg>';
function updateMuteBtn(){ if(!muteBtn || !vid) return; muteBtn.innerHTML = vid.muted ? MUTED_SVG : UNMUTED_SVG; muteBtn.title = vid.muted ? STRINGS.unmute : STRINGS.mute; }

// One-time-per-watch sound decision, run on the first `playing` (reset by setVideo).
let soundHandled=false;
function handleSoundOnPlaying(){
  if(soundHandled || !vid) return; soundHandled=true;
  if(!vid.muted){ hideSoundPill(); return; }
  const pref=soundPref();
  if(pref.muted===false){
    // The user chose sound before — try to restore it. A programmatic unmute (no
    // gesture) can be blocked: muted snaps back, the volume write throws, or the
    // element pauses. Any of those → re-mute, keep playing, fall back to the pill.
    try{ vid.muted=false; vid.volume=pref.volume; }catch(e){}
    setTimeout(()=>{
      if(vid.muted || vid.paused){ try{ vid.muted=true; }catch(e){} if(vid.paused) vid.play().catch(()=>{}); showSoundPill(); }
      else hideSoundPill();
      updateMuteBtn();
    }, 60);
  } else {
    showSoundPill();
  }
  updateMuteBtn();
}

if(soundPill) soundPill.addEventListener('click', ()=>{
  const pref=soundPref();
  try{ vid.muted=false; vid.volume=pref.volume; }catch(e){}
  hideSoundPill(); saveSoundPref(false, vid.volume); updateMuteBtn();
});
if(muteBtn) muteBtn.addEventListener('click', ()=>{
  try{ vid.muted=!vid.muted; }catch(e){}
  if(!vid.muted) hideSoundPill();
  saveSoundPref(vid.muted, vid.volume); updateMuteBtn();
});
if(vid){
  vid.addEventListener('playing', ()=>{ cancelStallLadder(); hideCatchup(); handleSoundOnPlaying(); });
  vid.addEventListener('volumechange', updateMuteBtn);
  updateMuteBtn();
}

/* ---- behind-live indicator + "Skip to live" ----
   Every ~2s while watching: compute the live-edge lag (hls.js latency on Android via
   the __hlsLatency seam; the seekable end minus currentTime on native HLS). Over 6s
   behind → append " · Ns behind" to the HUD mode text and show the skip chip. */
let behindSuffix='';
export function getBehindSuffix(){ return behindSuffix; } // applyStats() re-appends this when it repaints modeText
function behindLiveSecs(){
  try{
    if(window.__hlsLatency){ const l=window.__hlsLatency(); return (typeof l==='number' && isFinite(l)) ? l : null; }
    if(vid && vid.seekable && vid.seekable.length) return vid.seekable.end(vid.seekable.length-1)-vid.currentTime;
  }catch(e){}
  return null;
}
const stripBehind=t=>(t||'').replace(/ · \d+s behind$/,'');
function updateBehindLive(){
  const watching=['live','seed','catchup'].includes(S.curState);
  const mt=document.getElementById('modeText');
  if(!watching){ behindSuffix=''; hideSkipChip(); return; }
  const b=behindLiveSecs();
  if(b!=null && b>6){ behindSuffix=STRINGS.behindLive(Math.round(b)); if(skipBtn) skipBtn.hidden=false; }
  else { behindSuffix=''; hideSkipChip(); }
  if(mt) mt.textContent=stripBehind(mt.textContent)+behindSuffix;
}
setInterval(updateBehindLive, 2000);

if(skipBtn) skipBtn.addEventListener('click', ()=>{
  try{
    if(window.__hlsSkipToLive){ window.__hlsSkipToLive(); }
    else if(vid && vid.seekable && vid.seekable.length){ vid.currentTime=Math.max(0, vid.seekable.end(vid.seekable.length-1)-1.5); }
  }catch(e){}
  behindSuffix=''; hideSkipChip();
  const mt=document.getElementById('modeText'); if(mt) mt.textContent=stripBehind(mt.textContent);
});
