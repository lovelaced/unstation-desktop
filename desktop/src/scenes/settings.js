// Settings scene: account / network-pass / backup-copy / connection rows,
// reflecting real state (chainReady, bulletinReady, mesh-status, live sessions).

import * as sso from '../sso.js';
import { invoke, NATIVE } from '../tauri.js';
import { S, go, netLabel } from '../state.js';
import { viewerVerdict } from '../health.js';
import { isVideoPlaying } from '../player.js';
import { STRINGS } from '../copy.js';

// Settings → Network + Connection health, reflecting real state.
export function updateSettingsStatus(){
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  // Network pass = the statement-store allowance that makes sign-in + mesh work.
  set('setAllow', S.chainReady ? 'Granted' : 'Not granted'); const ad=document.getElementById('setAllowDot'); if(ad) ad.dataset.h = S.chainReady ? 'good' : 'wait';
  const gb=document.getElementById('grantAccessBtn'); if(gb) gb.style.display = S.chainReady ? 'none' : '';
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
  let signedIn = false; try { signedIn = await sso.awaitSession(); } catch(e){}
  el.textContent = S.chainReady ? 'Signed in' : (signedIn ? 'Paired — network access pending' : 'Not signed in');
  updateSettingsStatus();
}
