// Settings scene: account / network-pass / backup-copy / connection rows,
// reflecting real state (chainReady, bulletinReady, mesh-status, live sessions).

import * as sso from '../sso.js';
import { invoke, NATIVE } from '../tauri.js';
import { S, go, netLabel } from '../state.js';
import { viewerVerdict } from '../health.js';
import { isVideoPlaying } from '../player.js';
import { STRINGS } from '../copy.js';

/* ---- preference controls (persisted in localStorage; pushed to the backend) ----
   - Sharing your connection: how much upload Unstation may use to pass streams along
     (never called "seeding"). 0 = Auto (health-gated); 1 = effectively off.
   - Camera quality: phone broadcasts only.
   - Fast connect: whether the broadcaster honors fast-connect invites. */
const isMobile = () => window.__unstationPlatformType === 'mobile' || document.body.classList.contains('plat-android');
{
  const explain=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  explain('lendExplain', STRINGS.lendExplain);
  explain('camQExplain', STRINGS.camQExplain);
  explain('fastSetExplain', STRINGS.fastSetExplain);
  explain('helpersExplain', STRINGS.helpersExplain);

  const cap=document.getElementById('lendCap');
  if(cap){
    cap.value = localStorage.getItem('lendCap') || '0';
    if(![...cap.options].some(o=>o.value===cap.value)) cap.value='0';
    cap.addEventListener('change', ()=>{
      localStorage.setItem('lendCap', cap.value);
      if(NATIVE && invoke) invoke('set_lend_cap', { bps: parseInt(cap.value,10)||0 }).catch(()=>{});
    });
  }

  const rowQ=document.getElementById('rowCamQuality');
  const q=document.getElementById('camQuality');
  if(rowQ) rowQ.hidden = !isMobile(); // camera broadcasts are phone-only
  if(q){
    q.value = localStorage.getItem('camQuality') || '720';
    if(![...q.options].some(o=>o.value===q.value)) q.value='720';
    // localStorage is the whole contract: the Android shim reads `camQuality` on
    // __onPublishStarted and passes it to camera_start({quality}) — so this DOES
    // take effect (at the next go-live), no backend push needed here.
    q.addEventListener('change', ()=>{ localStorage.setItem('camQuality', q.value); });
  }

  // Fast connect is a broadcaster capability of the desktop (WHIP) publish path.
  const rowF=document.getElementById('rowFastConnect');
  if(rowF) rowF.hidden = isMobile();
  const ft=document.getElementById('fastConnectToggle');
  if(ft){
    const render=()=>{
      const off = localStorage.getItem('fastConnect')==='off';
      const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
      set('setFast', off ? STRINGS.fastSetOff : STRINGS.fastSetOn);
      const d=document.getElementById('setFastDot'); if(d) d.dataset.h = off ? '' : 'good';
      ft.textContent = off ? 'Turn on' : 'Turn off';
    };
    render();
    ft.addEventListener('click', ()=>{
      const off = localStorage.getItem('fastConnect')==='off';
      localStorage.setItem('fastConnect', off ? 'on' : 'off');
      if(NATIVE && invoke) invoke('set_fast_connect', { allowed: off }).catch(()=>{});
      render();
    });
  }

  // Volunteer relays: whether a broadcast may recruit volunteer seeds to help carry
  // it. Broadcaster-relevant on BOTH platforms (phones go live too), so unlike fast
  // connect the row is never hidden. Default on; origin-shield forces the effective
  // value on in the backend regardless of this preference.
  const ht=document.getElementById('helpersToggle');
  if(ht){
    const render=()=>{
      const off = localStorage.getItem('useHelpers')==='off';
      const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
      set('setHelpers', off ? STRINGS.helpersOff : STRINGS.helpersOn);
      const d=document.getElementById('setHelpersDot'); if(d) d.dataset.h = off ? '' : 'good';
      ht.textContent = off ? 'Turn on' : 'Turn off';
    };
    render();
    ht.addEventListener('click', ()=>{
      const off = localStorage.getItem('useHelpers')==='off';
      localStorage.setItem('useHelpers', off ? 'on' : 'off');
      if(NATIVE && invoke) invoke('set_use_helpers', { allowed: off }).catch(()=>{});
      render();
    });
  }
}

// Chain write health (U2), sampled on each settings open: failures that grew since
// the LAST open, or a dropped subscription while signed in, mean the pass is granted
// but not actually working — show it degraded with the Grant access recovery button.
let lastPassFailures = null, passDegraded = false;

// Settings → Network + Connection health, reflecting real state.
export function updateSettingsStatus(){
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  // Network pass = the statement-store allowance that makes sign-in + mesh work.
  const ad=document.getElementById('setAllowDot');
  const gb=document.getElementById('grantAccessBtn');
  if(S.chainReady && passDegraded){
    set('setAllow', STRINGS.settingsPassDegraded); if(ad) ad.dataset.h='wait';
    if(gb) gb.style.display='';
  } else {
    set('setAllow', S.chainReady ? 'Granted' : 'Not granted'); if(ad) ad.dataset.h = S.chainReady ? 'good' : 'wait';
    if(gb) gb.style.display = S.chainReady ? 'none' : '';
  }
  // Backup copy = the on-chain copy that lets viewers still find the stream if the
  // broadcaster drops out. Signed by the (optional) Bulletin allowance when granted.
  const bd=document.getElementById('setBackupDot');
  if(S.bulletinReady){ set('setBackup', STRINGS.backupOn); if(bd) bd.dataset.h='good'; }
  else { set('setBackup', S.chainReady ? STRINGS.backupOff : '—'); if(bd) bd.dataset.h=''; }
  const nl=netLabel(); set('setNetwork', nl.t); const nd=document.getElementById('setNetDot'); if(nd) nd.dataset.h=nl.h;
  let ht='Not watching or streaming right now.', hh='';
  if(S.publishing){ ht = S.pubLive ? ('Streaming live · '+S.lastViewers+' watching') : 'Stream open · waiting for your encoder'; hh = S.pubLive?'good':'wait'; }
  else if(['live','seed','catchup'].includes(S.curState)){ const v=viewerVerdict(S.lastPeers, isVideoPlaying('vid')); ht = v.label + ' · ' + S.lastPeers + ' ' + (S.lastPeers===1?'peer':'peers'); hh = v.dot; }
  set('setHealth', ht); const hd=document.getElementById('setHealthDot'); if(hd) hd.dataset.h=hh;
}

// Lending bandwidth (seed-by-default, health-gated) — driven by the `seed-stats`
// event: contribution level while watching, and the background-seed meter after
// leaving a stream (with its Stop control).
export function applySeedStats(p){
  if(!p) return;
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  const dot=document.getElementById('setLendDot');
  const stop=document.getElementById('stopSeedBtn');
  if(stop) stop.style.display = p.seeding ? '' : 'none';
  const up = p.uplink_kbps>=1000 ? (p.uplink_kbps/1000).toFixed(1)+' Mbps' : p.uplink_kbps+' kbps';
  if(p.seeding){
    set('setLend', 'Helping carry '+p.stream+' \u00b7 '+up+' up \u00b7 '+p.peers+' '+(p.peers===1?'peer':'peers'));
    if(dot) dot.dataset.h='good';
  } else if(p.level==='paused'){
    set('setLend', STRINGS.lendPaused);
    if(dot) dot.dataset.h='wait';
  } else if(p.level==='off'){
    set('setLend', STRINGS.lendOff);
    if(dot) dot.dataset.h='';
  } else {
    set('setLend', (p.level==='reduced' ? 'On (reduced \u2014 your connection is busy)' : 'On while you watch')+' \u00b7 '+up+' up');
    if(dot) dot.dataset.h='good';
  }
}

// Use the async session check (the sync one races storage hydration and wrongly
// shows "Not signed in" right after pairing). Show a transient "Checking…" first.
export async function openSettings(){
  go('settings');
  updateSettingsStatus();
  const el = document.getElementById('setAccount'); el.textContent = 'Checking…';
  // Reflect the LIVE connection state — the mesh-status event is one-shot, so read the
  // current subscription status each time Settings opens rather than trusting stale state.
  if(NATIVE && invoke){ try{ S.chainState = await invoke('chain_status'); }catch(e){} }
  // Same beat: has the network pass actually been WORKING since the last open?
  if(NATIVE && invoke){
    try{
      const h = await invoke('chain_health');
      const grew = lastPassFailures!=null && h.write_failures>lastPassFailures;
      lastPassFailures = h.write_failures;
      passDegraded = grew || (!h.subscribed && S.chainReady);
    }catch(e){}
  }
  let signedIn = false; try { signedIn = await sso.awaitSession(); } catch(e){}
  el.textContent = S.chainReady ? 'Signed in' : (signedIn ? 'Paired · network access pending' : 'Not signed in');
  updateSettingsStatus();
}
