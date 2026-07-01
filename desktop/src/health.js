// Viewer connection health. A plain-language verdict + a DOM renderer.
import { STRINGS } from './copy.js';
// We only have REAL signals (peer count + whether video is actually playing) — so we
// never show fake precision (no invented latency/%/ICE type). The "your connection"
// popup and the always-on pill dot share this verdict.
export function viewerVerdict(peers, playing){
  if(peers<=0) return { label:'Connecting…', note:'Finding people streaming this near you — make sure the broadcaster is live.', dot:'wait' };
  if(!playing) return { label:'Almost there', note:'Connected to '+peers+' '+(peers===1?'person':'people')+' — pulling in the video now.', dot:'wait' };
  if(peers>=2) return { label:'Excellent', note:'Smooth and steady — streaming from '+peers+' people, with no server in the middle.', dot:'good' };
  return { label:'Good', note:'Streaming directly from 1 person. More viewers nearby will make it sturdier.', dot:'ok' };
}
export function renderViewerHealth(o){
  o = o || {}; const peers = o.peers||0; const v = viewerVerdict(peers, !!o.playing);
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  set('vHealthLabel', v.label); set('vHealthNote', v.note);
  set('vPeers', peers + ' ' + (peers===1?'person':'people'));
  set('vSource', o.mode==='seed' ? STRINGS.sourceHelper : STRINGS.sourcePeers);
  if(o.publisher) set('mPub', o.publisher);
  set('peerCount', String(peers));
  const vd=document.getElementById('vHealthDot'); if(vd) vd.dataset.h=v.dot;
  const pd=document.querySelector('#pill .health-dot'); if(pd) pd.dataset.h=v.dot;
}
