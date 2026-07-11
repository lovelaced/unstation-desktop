// Viewer connection health. A plain-language verdict + a DOM renderer.
import { STRINGS } from './copy.js';
// We only have REAL signals (peer count + whether video is actually playing) — so we
// never show fake precision (no invented latency/%/ICE type). The "your connection"
// popup and the always-on pill dot share this verdict.
export function viewerVerdict(peers, playing){
  if(peers<=0) return { label:'Connecting…', note:'Finding people streaming this near you. Make sure the broadcaster is live.', dot:'wait' };
  if(!playing) return { label:'Almost there', note:'Connected to '+peers+' '+(peers===1?'person':'people')+', pulling in the video now.', dot:'wait' };
  if(peers>=2) return { label:'Excellent', note:'Smooth and steady, streaming from '+peers+' people with no server in the middle.', dot:'good' };
  return { label:'Good', note:'Streaming directly from 1 person. More viewers nearby will make it sturdier.', dot:'ok' };
}
// Verdict stability latch: stats arrive every couple of seconds and a peer count
// hovering on a threshold (0↔1, 1↔2) would flap the dot color and copy with it.
// A changed verdict must repeat on two consecutive updates before the dot/label
// swap — real transitions land within ~2 updates, boundary noise never does.
// Peer counts themselves always render (they're facts, not verdicts).
let shownVerdict = null, pendingDot = '';
export function renderViewerHealth(o){
  o = o || {}; const peers = o.peers||0; const v = viewerVerdict(peers, !!o.playing);
  if(!shownVerdict || v.dot === shownVerdict.dot){ shownVerdict = v; pendingDot = ''; }
  else if(v.dot === pendingDot){ shownVerdict = v; pendingDot = ''; } // confirmed twice
  else { pendingDot = v.dot; }                                       // first sighting: hold
  const sv = shownVerdict;
  const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
  set('vHealthLabel', sv.label); set('vHealthNote', sv.note);
  set('vPeers', peers + ' ' + (peers===1?'person':'people'));
  set('vSource', o.mode==='seed' ? STRINGS.sourceBackup : STRINGS.sourcePeers);
  if(o.publisher) set('mPub', o.publisher);
  set('peerCount', String(peers));
  const vd=document.getElementById('vHealthDot'); if(vd) vd.dataset.h=sv.dot;
  const pd=document.querySelector('#pill .health-dot'); if(pd) pd.dataset.h=sv.dot;
}
