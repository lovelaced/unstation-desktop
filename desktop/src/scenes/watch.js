// Watch (viewer): the finding sequence, live stats, self-preview, and the
// enter/leave lifecycle. Registers runFinding() as state.js's finding hook.
// On native the finding steps are driven by REAL `watch-phase` events from the
// backend (see applyWatchPhase); the preview build keeps the timed simulation.

import { invoke, NATIVE } from '../tauri.js';
import { S, go, win, setTitle, clearSeq, addSeqTimer, setTtff, registerFindingHook } from '../state.js';
import { renderViewerHealth } from '../health.js';
import { STRINGS } from '../copy.js';
import { setVideo, isVideoPlaying, startWatchWatchdog, clearWatchUi, setCatchup, hideCatchup, cancelStallLadder, getBehindSuffix } from '../player.js';
import { updateSettingsStatus } from './settings.js';
import { ensureSignedIn } from './onboarding.js';
import { shareName } from './publish.js';
import { toggleFullscreen } from '../main.js';

function materialize(){}   /* retained as a no-op — runFinding() still calls it */

/* ---- finding scene ---- */
// TTFF counter: starts at runFinding, re-syncs to the backend clock from the first
// watch-phase event's since_ms, and freezes at its current value when `live` lands.
let ttffStart=0, ttffFrozen=true, ttffSynced=false, stepReached=-1;
const STEP_ORDER=['resolve','verify','peers','first'];
function findingSteps(){ return [...document.querySelectorAll('#steps .step')]; }
// Forward-only per finding run: the backend's phase watcher and trust gate race
// (e.g. `discovering` can tick out before `verifying` lands), and progress that
// jumps backwards reads as a glitch — so a lower step never demotes a higher one.
function setFindingStep(activeKey){ // everything before activeKey done, activeKey active
  const idx=STEP_ORDER.indexOf(activeKey);
  if(idx<stepReached) return; stepReached=idx;
  findingSteps().forEach(s=>{ const i=STEP_ORDER.indexOf(s.dataset.k); s.className='step'+(i<idx?' done':(i===idx?' active':'')); });
}
function findingCopy(eyebrow,title,sub){
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  set('progEyebrow',eyebrow); set('progTitle',title); set('progSub',sub);
}

export function runFinding(){ materialize(); findingSteps().forEach(s=>s.className='step');
  findingCopy('Connecting','Finding the stream',"Resolving the name and checking the publisher's signature.");
  setTitle(S.watchTarget||'hardfork.dot',false);
  ttffStart=performance.now(); ttffFrozen=false; ttffSynced=false; stepReached=-1;
  const ttffEl=document.getElementById('ttff'); const tick=()=>{ if(ttffFrozen) return; ttffEl.textContent=((performance.now()-ttffStart)/1000).toFixed(1); setTtff(requestAnimationFrame(tick)); }; tick();
  if(NATIVE){ setFindingStep('resolve'); return; } // real progression arrives via applyWatchPhase
  /* preview only: the old timed simulation */
  const at=[350,800,1500,2300];
  STEP_ORDER.forEach((k,i)=>addSeqTimer(setTimeout(()=>{ setFindingStep(k);
    if(k==='peers'){ findingCopy(STRINGS.joiningEyebrow,STRINGS.joiningTitle,STRINGS.joiningSub); } },at[i])));
  addSeqTimer(setTimeout(()=>{ findingSteps().forEach(s=>s.className='step done'); },2900));
  addSeqTimer(setTimeout(()=>go('live'),3200)); }
registerFindingHook(runFinding);

/* ---- watch-phase: the honest viewer state machine (subscribed in main.js boot) ---- */
function enterLiveFromFinding(){
  findingSteps().forEach(s=>s.className='step done');
  const ttffEl=document.getElementById('ttff'); if(ttffEl && !ttffFrozen) ttffEl.textContent=((performance.now()-ttffStart)/1000).toFixed(1);
  ttffFrozen=true; // freeze at the real time-to-first-frame; go() cancels the rAF
  go('live');
  if(S.watchTarget) setTitle(S.watchTarget,true);
}
// The broadcast is over (terminal from the backend): tear the player down WITHOUT
// leaveWatch (which also navigates to entry) and land on the ended scene. stop_watch
// is fired once here; Rejoin runs a full start_watch (which tears down first anyway).
function endWatchToEnded(){
  if(S.fsOn) toggleFullscreen();
  if(NATIVE && invoke){ invoke('stop_watch').catch(()=>{}); }
  cleanupVideo();
  go('ended');
}
export function applyWatchPhase(p){
  if(!NATIVE || !p || !p.phase) return;
  const finding = S.curState==='finding';
  const watching = ['live','seed','catchup'].includes(S.curState);
  if(!finding && !watching) return; // stale event after leaving the watch
  if(finding && !ttffSynced && typeof p.since_ms==='number'){ ttffStart=performance.now()-p.since_ms; ttffSynced=true; }
  switch(p.phase){
    case 'resolving':   if(finding) setFindingStep('resolve'); break;
    case 'verifying':   if(finding) setFindingStep('verify'); break;
    case 'discovering':
    case 'connecting':
      if(finding){ setFindingStep('peers'); findingCopy(STRINGS.joiningEyebrow,STRINGS.joiningTitle,STRINGS.joiningSub); }
      break;
    case 'buffering':
      if(finding){ setFindingStep('first'); findingCopy(STRINGS.joiningEyebrow,STRINGS.joiningTitle,STRINGS.joiningSub); }
      break;
    case 'live':
      if(finding) enterLiveFromFinding();
      cancelStallLadder(); hideCatchup(); // any live edge clears the catchup story
      break;
    case 'catching-up':
      if(watching) setCatchup('<span class="spin"></span>'+STRINGS.catchingUp);
      break;
    case 'unreachable':
      // Non-terminal: the backend keeps retrying — say so honestly, keep the TTFF
      // ticking, and let a later phase resume the steps.
      if(finding) findingCopy(STRINGS.unreachableEyebrow,STRINGS.unreachableTitle,STRINGS.unreachableSub);
      break;
    case 'ended':
      endWatchToEnded();
      break;
  }
}

/* real stats from the engine */
export function applyStats(s){ if(!s)return; S.lastPeers = s.peers||0; const seed=s.mode==='seed'; win.dataset.health=seed?'seed':'p2p';
  const mt=document.getElementById('modeText'); if(mt) mt.textContent=(seed?STRINGS.modeLiveHelper:STRINGS.modeLiveP2p)+getBehindSuffix();
  renderViewerHealth({peers:S.lastPeers, playing:isVideoPlaying('vid'), mode:s.mode});
  if(S.curState==='settings') updateSettingsStatus(); }

// Self-check: watch your OWN stream on this machine. A same-identity viewer can't
// discover itself over the mesh, so this plays your local publish feed directly —
// verifying encoder → ingest → segmenter → HLS → player end to end on one device.
export function selfWatch(name){
  if(!(NATIVE && S.pubHlsUrl)) return;
  go('live'); setTitle(name+' · preview', true); S.lastPeers=0;
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  set('mPub', name+' (you)'); set('vPeers','you'); set('vSource','your local feed');
  set('vHealthLabel','Your own stream'); set('vHealthNote','Local preview of what you’re broadcasting — confirms your encoder and video pipeline are working.');
  const vd=document.getElementById('vHealthDot'); if(vd) vd.dataset.h='good';
  set('peerCount','0'); const pd=document.querySelector('#pill .health-dot'); if(pd) pd.dataset.h='good';
  const mt=document.getElementById('modeText'); if(mt) mt.textContent='PREVIEW · you';
  setVideo(S.pubHlsUrl);
}

// Start (or re-start) watching a stream. The single path shared by the watch form,
// the ended scene's Rejoin, and the give-up card's Try again. On native we STAY on
// the finding scene until the `live` watch-phase arrives — but the player warms up
// behind it (setVideo + the stall ladder start as soon as start_watch resolves).
export async function startWatch(target){
  target=(target||'').trim(); if(!target) return;
  if(!ensureSignedIn()) return;
  if(NATIVE && S.publishing && S.pubName && shareName(target)===S.pubName){ selfWatch(S.pubName); return; }
  S.watchTarget=target;
  go('finding');
  if(NATIVE && invoke){ try{ const info=await invoke('start_watch',{ target }); document.getElementById('mPub').textContent=info.publisher; S.lastPeers=0; setTitle(target,true); applyStats({peers:0,rho:0,mode:'p2p',from_seed:0,from_chain:0,latency_s:0,ice:'connecting'}); startWatchWatchdog(); setVideo(info.hls_url); }catch(err){ console.error('start_watch failed',err); findingCopy('Problem','Couldn’t start watching',((err&&err.message)||(''+err))); clearSeq(); } }
  else { setTimeout(()=>go('live'),1200); }
}

document.getElementById('watchForm').addEventListener('submit', (e)=>{ e.preventDefault(); const target=(document.getElementById('streamInput').value||'').trim(); if(!target){ document.getElementById('streamInput').focus(); return; } startWatch(target); });

// The give-up card's buttons are injected HTML (player.js's 45s rung) — delegate.
document.getElementById('catchup').addEventListener('click', async (e)=>{
  const b=e.target && e.target.closest ? e.target.closest('button') : null; if(!b) return;
  if(b.id==='retryWatchBtn'){ if(NATIVE && invoke){ try{ await invoke('stop_watch'); }catch(err){} } if(S.watchTarget) startWatch(S.watchTarget); else go('entry'); }
  else if(b.id==='giveUpLeaveBtn'){ leaveWatch(); }
});

// Double-click the video frame → toggle fullscreen (same control as the HUD button).
{ const fr=document.querySelector('#player .frame'); if(fr) fr.addEventListener('dblclick', ()=>{ toggleFullscreen(); }); }

// Watch tab — re-attach to a running watch, else go to the browse/entry screen.
export async function enterWatch(){
  if(!ensureSignedIn()) return;
  let status = null;
  if(NATIVE && invoke){ try{ status = await invoke('watch_status'); }catch(e){} }
  if(status){ document.getElementById('mPub').textContent = status.info.publisher; go('live'); setTitle(status.info.publisher, true); startWatchWatchdog(); setVideo(status.info.hls_url); }
  else { go('entry'); }
}

// Player teardown shared by Leave and the ended phase: kill the retry loop + every
// ladder timer (clearWatchUi), stop and detach the media. Does NOT stop_watch.
function cleanupVideo(){ const v=document.getElementById('vid'); clearInterval(v._retry); clearWatchUi(); try{ v.pause(); }catch(e){} v.removeAttribute('src'); v.style.display='none'; }

export async function leaveWatch(){ if(S.fsOn) toggleFullscreen(); if(NATIVE && invoke){ try{ await invoke('stop_watch'); }catch(e){} } cleanupVideo(); go('entry'); }
