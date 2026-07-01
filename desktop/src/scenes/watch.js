// Watch (viewer): the finding sequence, live stats, self-preview, and the
// enter/leave lifecycle. Registers runFinding() as state.js's finding hook.

import { invoke, NATIVE } from '../tauri.js';
import { S, go, win, setTitle, clearSeq, addSeqTimer, setTtff, registerFindingHook } from '../state.js';
import { renderViewerHealth } from '../health.js';
import { STRINGS } from '../copy.js';
import { setVideo, isVideoPlaying, startWatchWatchdog, clearWatchUi } from '../player.js';
import { updateSettingsStatus } from './settings.js';
import { ensureSignedIn } from './onboarding.js';
import { shareName } from './publish.js';
import { toggleFullscreen } from '../main.js';

function materialize(){}   /* retained as a no-op — runFinding() still calls it */

export function runFinding(){ materialize(); const steps=[...document.querySelectorAll('#steps .step')]; steps.forEach(s=>s.className='step');
  document.getElementById('progTitle').textContent='Finding the stream'; document.getElementById('progEyebrow').textContent='Connecting';
  document.getElementById('progSub').textContent="Resolving the name and checking the publisher's signature."; setTitle('hardfork.dot',false);
  const t0=performance.now(), ttffEl=document.getElementById('ttff'); const tick=()=>{ttffEl.textContent=((performance.now()-t0)/1000).toFixed(1); setTtff(requestAnimationFrame(tick));}; tick();
  const order=['resolve','verify','peers','first'], at=[350,800,1500,2300];
  order.forEach((k,i)=>addSeqTimer(setTimeout(()=>{ steps.forEach(s=>{if(s.dataset.k===k)s.classList.add('active');}); if(i>0)steps[i-1].classList.replace('active','done');
    if(k==='peers'){ document.getElementById('progTitle').textContent='Joining the mesh'; document.getElementById('progEyebrow').textContent='Almost there'; document.getElementById('progSub').textContent='Connecting to peers near you over WebRTC.'; } },at[i])));
  addSeqTimer(setTimeout(()=>steps[3].classList.replace('active','done'),2900));
  if(!NATIVE) addSeqTimer(setTimeout(()=>go('live'),3200)); }
registerFindingHook(runFinding);

/* real stats from the engine */
export function applyStats(s){ if(!s)return; S.lastPeers = s.peers||0; const seed=s.mode==='seed'; win.dataset.health=seed?'seed':'p2p';
  const mt=document.getElementById('modeText'); if(mt) mt.textContent=seed?STRINGS.modeLiveHelper:STRINGS.modeLiveP2p;
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

document.getElementById('watchForm').addEventListener('submit', async (e)=>{ e.preventDefault(); const target=(document.getElementById('streamInput').value||'').trim(); if(!target){ document.getElementById('streamInput').focus(); return; } if(!ensureSignedIn()) return; if(NATIVE && S.publishing && S.pubName && shareName(target)===S.pubName){ selfWatch(S.pubName); return; } go('finding');
  if(NATIVE && invoke){ try{ const info=await invoke('start_watch',{ target }); document.getElementById('mPub').textContent=info.publisher; S.lastPeers=0; go('live'); setTitle(target,true); applyStats({peers:0,rho:0,mode:'p2p',from_seed:0,from_chain:0,latency_s:0,ice:'connecting'}); startWatchWatchdog(); setVideo(info.hls_url); }catch(err){ console.error('start_watch failed',err); document.getElementById('progEyebrow').textContent='Problem'; document.getElementById('progTitle').textContent='Couldn’t start watching'; document.getElementById('progSub').textContent=((err&&err.message)||(''+err)); clearSeq(); } }
  else { setTimeout(()=>go('live'),1200); } });

// Watch tab — re-attach to a running watch, else go to the browse/entry screen.
export async function enterWatch(){
  if(!ensureSignedIn()) return;
  let status = null;
  if(NATIVE && invoke){ try{ status = await invoke('watch_status'); }catch(e){} }
  if(status){ document.getElementById('mPub').textContent = status.info.publisher; go('live'); setTitle(status.info.publisher, true); startWatchWatchdog(); setVideo(status.info.hls_url); }
  else { go('entry'); }
}

export async function leaveWatch(){ if(S.fsOn) toggleFullscreen(); if(NATIVE && invoke){ try{ await invoke('stop_watch'); }catch(e){} } const v=document.getElementById('vid'); clearInterval(v._retry); clearWatchUi(); try{ v.pause(); }catch(e){} v.removeAttribute('src'); v.style.display='none'; go('entry'); }
