import './wasm-compat.js'; // must precede sso.js — neutralizes WKWebView's broken wasm streaming
import QRCode from 'qrcode';
import * as sso from './sso.js';
import { invoke, listen, NATIVE, appWindow } from './tauri.js';
import { initAmbient } from './ambient.js';
import { viewerVerdict, renderViewerHealth } from './health.js';

(() => {
  const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const win = document.getElementById('win');

  // Ambient warm-glow field — see ./ambient.js. Wires the blur/visibility pause and
  // returns the per-scene visibility toggle the state machine calls.
  const setAmbient = initAmbient(win);
  function materialize(){}   /* retained as a no-op — runFinding() still calls it */

  /* ---- state machine ---- */
  const scenes=[...document.querySelectorAll('.scene')], player=document.getElementById('player'), hud=document.getElementById('hud');
  const titleCenter=document.getElementById('titleCenter'); let seqTimers=[], ttffTimer=null, curStateName='entry';
  function clearSeq(){ seqTimers.forEach(clearTimeout); seqTimers=[]; if(ttffTimer)cancelAnimationFrame(ttffTimer); }
  function showScene(name){ scenes.forEach(s=>s.classList.toggle('show', s.dataset.scene===name)); }
  function setTitle(stream,verified){ titleCenter.style.opacity=0; setTimeout(()=>{ if(verified){ titleCenter.innerHTML='<span class="verified"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 12l2 2 4-4"/><circle cx="12" cy="12" r="9"/></svg></span>'+stream; } else { titleCenter.textContent=stream; } titleCenter.style.opacity=1; }, 200); }

  // Persistent top-level tabs. The current section is always highlighted; switching
  // tabs never stops a running session (see enterGoLive/enterWatch + endPublish).
  const tabsEl=document.getElementById('tabs'), tabEls=[...document.querySelectorAll('.tab')], goLiveRec=document.getElementById('goLiveRec');
  let pubActive=false; // a publish session exists in the backend (live or waiting)
  function tabForState(state){ if(state==='publish')return 'golive'; if(state==='settings')return 'settings'; if(state==='onboarding')return null; return 'watch'; }
  function setActiveTab(state){ if(tabsEl) tabsEl.hidden=(state==='onboarding'); const t=tabForState(state); tabEls.forEach(b=>b.classList.toggle('active', b.dataset.tab===t)); }
  function refreshGoLiveBadge(){ if(goLiveRec) goLiveRec.hidden=!pubActive; }
  function go(state){ clearSeq(); curStateName=state;
    document.querySelectorAll('.dock button').forEach(b=>b.classList.toggle('on', b.dataset.state===state));
    const isLive=['live','seed','catchup'].includes(state);
    setActiveTab(state);
    player.classList.toggle('show', isLive); hud.classList.toggle('show', isLive);
    document.getElementById('catchup').style.display = state==='catchup'?'grid':'none';
    if(isLive){ const mode=state==='seed'?'seed':'p2p'; win.dataset.health=mode; setAmbient(false); document.getElementById('modeText').textContent=state==='seed'?'LIVE · via relay':'LIVE · P2P'; showScene(''); if(!NATIVE){ setTitle('hardfork.dot',true); renderViewerHealth({peers: state==='seed'?6:23, playing:true, mode, publisher:'hardfork.dot'}); } return; }
    setAmbient(state==='entry'||state==='onboarding'||state==='ended'||state==='settings'); win.dataset.net='closed'; if(state!=='finding') setTitle('Unstation',false);
    if(state==='finding'){ showScene('finding'); runFinding(); return; } showScene(state); }

  function runFinding(){ materialize(); const steps=[...document.querySelectorAll('#steps .step')]; steps.forEach(s=>s.className='step');
    document.getElementById('progTitle').textContent='Finding the stream'; document.getElementById('progEyebrow').textContent='Connecting';
    document.getElementById('progSub').textContent="Resolving the name and checking the publisher's signature."; setTitle('hardfork.dot',false);
    const t0=performance.now(), ttffEl=document.getElementById('ttff'); const tick=()=>{ttffEl.textContent=((performance.now()-t0)/1000).toFixed(1); ttffTimer=requestAnimationFrame(tick);}; tick();
    const order=['resolve','verify','peers','first'], at=[350,800,1500,2300];
    order.forEach((k,i)=>seqTimers.push(setTimeout(()=>{ steps.forEach(s=>{if(s.dataset.k===k)s.classList.add('active');}); if(i>0)steps[i-1].classList.replace('active','done');
      if(k==='peers'){ document.getElementById('progTitle').textContent='Joining the mesh'; document.getElementById('progEyebrow').textContent='Almost there'; document.getElementById('progSub').textContent='Connecting to peers near you over WebRTC.'; } },at[i])));
    seqTimers.push(setTimeout(()=>steps[3].classList.replace('active','done'),2900));
    if(!NATIVE) seqTimers.push(setTimeout(()=>go('live'),3200)); }

  function isVideoPlaying(id){ const v=document.getElementById(id); return !!(v && v.readyState>=3 && !v.paused && !v.ended); }
  // viewerVerdict + renderViewerHealth now live in ./health.js (imported above).

  /* real stats from the engine */
  let lastPeers = 0, watchWatchdog = null;
  function applyStats(s){ if(!s)return; lastPeers = s.peers||0; const seed=s.mode==='seed'; win.dataset.health=seed?'seed':'p2p';
    const mt=document.getElementById('modeText'); if(mt) mt.textContent=seed?'LIVE · via relay':'LIVE · P2P';
    renderViewerHealth({peers:lastPeers, playing:isVideoPlaying('vid'), mode:s.mode});
    if(curStateName==='settings') updateSettingsStatus(); }

  // Viewer-facing connection feedback. The catchup overlay carries the message; the
  // watchdog turns a silent "no video" into a peer-aware, actionable explanation.
  function setCatchup(html){ const c=document.getElementById('catchup'); if(c){ c.innerHTML=html; c.style.display='grid'; } }
  function clearWatchUi(){ clearTimeout(watchWatchdog); const c=document.getElementById('catchup'); if(c){ c.style.display='none'; } }
  function startWatchWatchdog(){
    clearTimeout(watchWatchdog);
    setCatchup('<span class="spin"></span>Connecting to the broadcaster…');
    watchWatchdog = setTimeout(()=>{
      const v=document.getElementById('vid'); if(v.readyState>=3 && !v.paused) return; // already playing
      if(lastPeers>0) setCatchup('Connected to a peer, but no video is arriving — the broadcaster may have stopped streaming.');
      else setCatchup('Can’t reach the broadcaster yet. Check the name matches exactly, that they’re live, and — for now — that both devices are on the same network.');
    }, 18000);
  }

  /* interactions */
  function toggleNet(){ if(!hud.classList.contains('show'))return; const open=win.dataset.net==='open'; win.dataset.net=open?'closed':'open'; }
  document.getElementById('pill').addEventListener('click',toggleNet);
  document.querySelectorAll('[data-goto]').forEach(b=>b.addEventListener('click',()=>go(b.dataset.goto)));

  // Attach the viewer to its local HLS server. The mesh delivers segments a moment
  // AFTER we attach, so the first playlist read is empty → the player reports
  // "error 4 / source unsupported". So we keep re-loading (cache-busted, to re-read
  // the now-growing playlist) until media actually plays, showing "Catching up…"
  // meanwhile. If it never plays, the peer count in the HUD tells the real story.
  function setVideo(url){
    const v=document.getElementById('vid'), catchup=document.getElementById('catchup'); if(!url)return;
    if(!(v.canPlayType('application/vnd.apple.mpegurl'))){ if(catchup){ catchup.textContent='This device can’t play the stream format.'; catchup.style.display='grid'; } return; }
    const attempt=()=>{ if(v.readyState>=3 && !v.paused) return; try{ v.src=url+(url.includes('?')?'&':'?')+'t='+Date.now(); v.style.display='block'; v.load(); v.play().catch(()=>{}); }catch(e){} };
    attempt();
    clearInterval(v._retry);
    v._retry=setInterval(()=>{ const playing=v.readyState>=3 && !v.paused; if(catchup && curStateName!=='catchup') catchup.style.display=playing?'none':'grid'; if(playing){ clearInterval(v._retry); clearTimeout(watchWatchdog); } else attempt(); }, 1500);
  }
  // Surface real media ERRORS on-screen (the bundled DMG has no devtools). We only
  // show genuine `error` events — `stalled`/`waiting` fire routinely at the live edge
  // during normal HLS playback, so showing those would be a constant false alarm.
  // `onScreen=false` (the viewer) logs to console only — a transient "error 4" is
  // EXPECTED while the mesh spins up (handled by setVideo's retry + the catchup
  // overlay), so flashing it on screen would just be alarming. The publisher's local
  // ingest, by contrast, has no retry, so its errors are shown.
  function wireVideoDiag(vidId, diagId, onScreen){
    const el=document.getElementById(vidId), diag=document.getElementById(diagId);
    if(!el) return;
    const CODES={1:'aborted',2:'network/blocked',3:'decode',4:'src unsupported'};
    const hide=()=>{ if(diag) diag.hidden=true; };
    el.addEventListener('error', ()=>{ const e=el.error; const m='video error '+((e&&e.code)||'?')+' ('+((e&&CODES[e.code])||'?')+')'+(e&&e.message?': '+e.message:'')+' — '+(el.currentSrc||'no src'); console.error('[video]',vidId,m); if(onScreen && diag){ diag.textContent=m; diag.hidden=false; } });
    el.addEventListener('playing', hide);
    el.addEventListener('loadeddata', hide);
    el.addEventListener('timeupdate', hide);
  }
  wireVideoDiag('pubVid','pubVidDiag', true);
  wireVideoDiag('vid','vidDiag', false);

  document.getElementById('watchForm').addEventListener('submit', async (e)=>{ e.preventDefault(); const target=(document.getElementById('streamInput').value||'').trim(); if(!target){ document.getElementById('streamInput').focus(); return; } if(!ensureSignedIn()) return; if(NATIVE && publishing && pubName && shareName(target)===pubName){ selfWatch(pubName); return; } go('finding');
    if(NATIVE && invoke){ try{ const info=await invoke('start_watch',{ target }); document.getElementById('mPub').textContent=info.publisher; lastPeers=0; go('live'); setTitle(target,true); applyStats({peers:0,rho:0,mode:'p2p',from_seed:0,from_chain:0,latency_s:0,ice:'connecting'}); startWatchWatchdog(); setVideo(info.hls_url); }catch(err){ console.error('start_watch failed',err); document.getElementById('progEyebrow').textContent='Problem'; document.getElementById('progTitle').textContent='Couldn’t start watching'; document.getElementById('progSub').textContent=((err&&err.message)||(''+err)); clearSeq(); } }
    else { setTimeout(()=>go('live'),1200); } });

  document.getElementById('pairedBtn').addEventListener('click', async ()=>{ if(NATIVE && invoke){ try{ await invoke('complete_signin'); }catch(e){} } go('entry'); });

  /* Go Live (publisher) — name your stream, share a friendly link, watch it back */
  let publishing = false, pubHlsUrl = null, pubViewersTimer = null, pubName = '';
  let pubLive = false, pubLiveSince = 0, lastViewers = 0, chainState = '', chainDetail = '', fsOn = false;
  // True once a usable, allowance-backed chain identity has been bridged to the backend
  // (set_chain_identity succeeded). A pairing *session* can exist without this, so gate
  // publish/watch and the "Signed in" status on chainReady, not on hasSession().
  let chainReady = false;
  let _identityInFlight = null; // single-flight guard so overlapping callers don't fire concurrent allocations
  let bulletinReady = false; // Bulletin allowance installed → durable-origin (manifest) writes sponsored
  function fmtDur(ms){ const s=Math.max(0,(ms/1000)|0), h=(s/3600)|0, m=((s%3600)/60)|0, ss=s%60; return (h>0?h+':'+String(m).padStart(2,'0'):String(m))+':'+String(ss).padStart(2,'0'); }
  // Plain network status shared by the Go Live card + Settings (driven by mesh-status).
  function netLabel(){
    if(chainState==='ready') return { t:'Connected', h:'good' };
    if(chainState==='connecting') return { t:'Connecting…', h:'wait' };
    if(chainState==='offline') return { t:'Not connected', h:'' };
    if(chainState==='error') return { t:(chainDetail||'Not connected'), h:'' };
    return { t: NATIVE?'Connecting…':'Preview', h:'wait' };
  }
  // Streamer-facing Go Live health: live/waiting + viewers + uptime + network. No jargon.
  function updatePubHealth(){
    const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
    const dot=document.getElementById('phDot');
    if(pubLive){ if(dot) dot.dataset.h='live'; set('phStatus','You’re live'); set('phNote','Your encoder is connected — viewers can tune in with your stream name.'); }
    else { if(dot) dot.dataset.h='wait'; set('phStatus','Waiting for your encoder'); set('phNote','Point OBS at the server above and start streaming — it goes live on its own.'); }
    set('phViewers', String(lastViewers));
    set('phUptime', pubLiveSince ? fmtDur(Date.now()-pubLiveSince) : '—');
    set('phNet', netLabel().t);
  }
  // Settings → Network + Connection health, reflecting real state.
  function updateSettingsStatus(){
    const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
    // Network access = the statement-store allowance that makes sign-in + mesh work.
    set('setAllow', chainReady ? 'Granted' : 'Not granted'); const ad=document.getElementById('setAllowDot'); if(ad) ad.dataset.h = chainReady ? 'good' : 'wait';
    const gb=document.getElementById('grantAccessBtn'); if(gb) gb.style.display = chainReady ? 'none' : '';
    // Durable backup = the on-chain copy that lets viewers still find the stream if the
    // broadcaster drops out. Signed by the (optional) Bulletin allowance when granted.
    const bd=document.getElementById('setBackupDot');
    if(bulletinReady){ set('setBackup', 'On — viewers can still find the stream if you drop'); if(bd) bd.dataset.h='good'; }
    else { set('setBackup', chainReady ? 'Off · optional backup' : '—'); if(bd) bd.dataset.h=''; }
    const nl=netLabel(); set('setNetwork', nl.t); const nd=document.getElementById('setNetDot'); if(nd) nd.dataset.h=nl.h;
    let ht='Not watching or streaming right now.', hh='';
    if(publishing){ ht = pubLive ? ('Streaming live · '+lastViewers+' watching') : 'Stream open · waiting for your encoder'; hh = pubLive?'good':'wait'; }
    else if(['live','seed','catchup'].includes(curStateName)){ const v=viewerVerdict(lastPeers, isVideoPlaying('vid')); ht = v.label + ' · ' + lastPeers + ' ' + (lastPeers===1?'peer':'peers'); hh = v.dot; }
    set('setHealth', ht); const hd=document.getElementById('setHealthDot'); if(hd) hd.dataset.h=hh;
  }
  // Live uptime ticker (cheap; only repaints while a stream is actually live).
  setInterval(()=>{ if(publishing && pubLive) updatePubHealth(); }, 1000);
  // Fullscreen for TV/laptop: hide the tabs + let the video fill the screen. Uses the
  // Tauri window's native fullscreen on desktop; falls back to the HTML API in preview.
  async function toggleFullscreen(){
    fsOn=!fsOn; win.classList.toggle('fs', fsOn);
    const lbl=document.getElementById('fsLabel'); if(lbl) lbl.textContent=fsOn?'Exit':'Fullscreen';
    if(appWindow){ try{ await appWindow.setFullscreen(fsOn); }catch(e){ console.error('[fs]',e); } }
    else { try{ if(fsOn){ win.requestFullscreen && win.requestFullscreen(); } else if(document.fullscreenElement){ document.exitFullscreen(); } }catch(e){} }
  }
  // Self-check: watch your OWN stream on this machine. A same-identity viewer can't
  // discover itself over the mesh, so this plays your local publish feed directly —
  // verifying encoder → ingest → segmenter → HLS → player end to end on one device.
  function selfWatch(name){
    if(!(NATIVE && pubHlsUrl)) return;
    go('live'); setTitle(name+' · preview', true); lastPeers=0;
    const set=(id,t)=>{ const el=document.getElementById(id); if(el) el.textContent=t; };
    set('mPub', name+' (you)'); set('vPeers','you'); set('vSource','your local feed');
    set('vHealthLabel','Your own stream'); set('vHealthNote','Local preview of what you’re broadcasting — confirms your encoder and video pipeline are working.');
    const vd=document.getElementById('vHealthDot'); if(vd) vd.dataset.h='good';
    set('peerCount','0'); const pd=document.querySelector('#pill .health-dot'); if(pd) pd.dataset.h='good';
    const mt=document.getElementById('modeText'); if(mt) mt.textContent='PREVIEW · you';
    setVideo(pubHlsUrl);
  }
  const slugify = t => ((t||'').trim().toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/^-+|-+$/g,'') || 'my-stream');
  // The stream's shareable name — just the text a viewer types. No ".dot": chain-
  // backed `.dot` resolution lands later, and the Rust side hashes the same
  // canonical name whether or not a ".dot" is present.
  const shareName = t => slugify(t);
  const titleEl = document.getElementById('streamTitle');
  const startStreamBtn = document.getElementById('startStream');
  titleEl.addEventListener('input', ()=>{ document.getElementById('sharePreview').textContent = shareName(titleEl.value); });

  // Step 1 — name the stream. We deliberately do NOT open the ingest yet: the
  // stream's identity is derived from this name, so it must be fixed before we
  // publish presence. (Opening on entry with an empty box made every stream
  // "my-stream" and broke discovery.)
  function showPubSetup(){
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
  async function goLiveStart(){
    if(!publishing) return;
    if(!ensureSignedIn()) return;
    pubName = shareName(titleEl.value);
    titleEl.value = pubName;
    titleEl.readOnly = true;
    document.getElementById('shareLink').textContent = pubName;
    document.getElementById('pubViewers').textContent = '0';
    document.getElementById('pubSetup').hidden = true;
    document.getElementById('pubLive').hidden = false;
    applyPublishState(false); // enter the console in the waiting state
    if(NATIVE && invoke){
      try {
        const info = await invoke('start_publish', { title: pubName });
        pubHlsUrl = info.hls_url; pubActive = true; refreshGoLiveBadge();
        document.getElementById('ingestServer').textContent = info.ingest_server;
        document.getElementById('ingestKey').textContent = info.stream_key;
      } catch(err){ const w=document.getElementById('pubWaiting'); const b=w&&w.querySelector('b'); if(b) b.textContent=''+err; }
    } else {
      pubActive = true; refreshGoLiveBadge();
      setTimeout(()=>{ if(publishing) applyPublishState(true); }, 1800); // preview demo
    }
  }

  // Reflect the REAL stream state — `publish-state` fires from the feeder based on
  // whether fresh fragments are actually arriving, so LIVE always matches the video.
  function applyPublishState(live){
    pubLive = live;
    if(live){ if(!pubLiveSince) pubLiveSince = Date.now(); } else { pubLiveSince = 0; }
    updatePubHealth();
    if(curStateName==='settings') updateSettingsStatus();
    // Only drive the on-screen console video when it's visible — a background publish
    // still emits publish-state while the user is on another tab.
    if(!publishing || document.getElementById('pubLive').hidden) return;
    const tag = document.getElementById('pubLiveTag');
    const waiting = document.getElementById('pubWaiting');
    const v = document.getElementById('pubVid');
    if(tag) tag.style.display = live ? '' : 'none';
    if(waiting) waiting.style.display = live ? 'none' : 'flex';
    document.getElementById('pubHeadline').textContent = live ? 'You’re live.' : 'Waiting for your encoder…';
    if(live){
      if(NATIVE && pubHlsUrl){ try{ if(v.canPlayType('application/vnd.apple.mpegurl')){ v.src=pubHlsUrl + (pubHlsUrl.includes('?')?'&':'?') + 't=' + Date.now(); v.style.display='block'; v.load(); v.play().catch(()=>{}); } }catch(e){} }
    } else {
      try{ v.pause(); }catch(e){} v.removeAttribute('src'); v.style.display='none';
    }
  }

  // Go Live tab — re-attach to a running stream (tab-back) instead of restarting it,
  // or open the name-entry step if nothing is publishing yet.
  // Sign-in is required to watch or publish — every mesh write (presence, signaling,
  // edge) needs the paired statement-store allowance. Soft-gate the entry points:
  // bounce to pairing when there's no live session yet. (Sync hasSession() is reliable
  // here — boot awaits hydration before any of these controls are reachable.)
  function ensureSignedIn(){
    if(!NATIVE) return true;
    if(chainReady) return true;
    // A pairing session may exist without a usable chain identity (allowance not yet
    // bridged, or a stale session). Reuse the session and retry the allowance WITHOUT
    // resetPairing (no slot churn); only fall back to a fresh pair if there's no session.
    go('onboarding');
    const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display='none';
    let have=false; try{ have=sso.hasSession(); }catch(e){}
    if(have){ onboardingStatus('Finishing network access on your phone…'); pushChainIdentity().then(ok=>{ if(ok){ onboardingStatus('Network access granted ✓'); setTimeout(()=>go('entry'), 700); } }); }
    else beginPairing();
    return false;
  }
  async function enterGoLive(){
    if(!ensureSignedIn()) return;
    go('publish');
    let status = null;
    if(NATIVE && invoke){ try{ status = await invoke('publish_status'); }catch(e){} }
    if(status){
      publishing = true; pubActive = true; pubName = status.name; pubHlsUrl = status.info.hls_url;
      titleEl.value = status.name; titleEl.readOnly = true;
      document.getElementById('ingestServer').textContent = status.info.ingest_server;
      document.getElementById('ingestKey').textContent = status.info.stream_key;
      document.getElementById('shareLink').textContent = status.name;
      lastViewers = status.viewers||0;
      document.getElementById('pubViewers').textContent = lastViewers;
      document.getElementById('pubSetup').hidden = true;
      document.getElementById('pubLive').hidden = false;
      refreshGoLiveBadge();
      applyPublishState(!!status.live);
    } else {
      publishing = true;
      showPubSetup();
      titleEl.focus(); if(titleEl.select) titleEl.select();
    }
  }
  // Watch tab — re-attach to a running watch, else go to the browse/entry screen.
  async function enterWatch(){
    if(!ensureSignedIn()) return;
    let status = null;
    if(NATIVE && invoke){ try{ status = await invoke('watch_status'); }catch(e){} }
    if(status){ document.getElementById('mPub').textContent = status.info.publisher; go('live'); setTitle(status.info.publisher, true); startWatchWatchdog(); setVideo(status.info.hls_url); }
    else { go('entry'); }
  }
  async function endPublish(){
    publishing = false; pubActive = false; pubName = ''; pubLive = false; pubLiveSince = 0; lastViewers = 0; clearInterval(pubViewersTimer);
    refreshGoLiveBadge();
    if(NATIVE && invoke){ try{ await invoke('stop_publish'); }catch(e){} }
    titleEl.readOnly = false;
    document.getElementById('pubLive').hidden = true;
    const tag=document.getElementById('pubLiveTag'); if(tag) tag.style.display='none';
    const v=document.getElementById('pubVid'); try{ v.pause(); }catch(e){} v.removeAttribute('src'); v.style.display='none';
    go('entry');
  }
  async function leaveWatch(){ if(fsOn) toggleFullscreen(); if(NATIVE && invoke){ try{ await invoke('stop_watch'); }catch(e){} } const v=document.getElementById('vid'); clearInterval(v._retry); clearWatchUi(); try{ v.pause(); }catch(e){} v.removeAttribute('src'); v.style.display='none'; go('entry'); }
  // Use the async session check (the sync one races storage hydration and wrongly
  // shows "Not signed in" right after pairing). Show a transient "Checking…" first.
  async function openSettings(){
    go('settings');
    updateSettingsStatus();
    const el = document.getElementById('setAccount'); el.textContent = 'Checking…';
    // Reflect the LIVE connection state — the mesh-status event is one-shot, so read the
    // current subscription status each time Settings opens rather than trusting stale state.
    if(NATIVE && invoke){ try{ chainState = await invoke('chain_status'); }catch(e){} }
    let signedIn = false; try { signedIn = await sso.awaitSession(); } catch(e){}
    el.textContent = chainReady ? 'Signed in' : (signedIn ? 'Paired — network access pending' : 'Not signed in');
    updateSettingsStatus();
  }

  document.getElementById('tabWatch').addEventListener('click', enterWatch);
  document.getElementById('tabGoLive').addEventListener('click', enterGoLive);
  document.getElementById('tabSettings').addEventListener('click', openSettings);
  document.getElementById('goLiveLink').addEventListener('click', enterGoLive);
  document.getElementById('leaveWatchBtn').addEventListener('click', leaveWatch);
  document.getElementById('fsBtn').addEventListener('click', toggleFullscreen);
  { const pv=document.getElementById('previewSelf'); if(pv) pv.addEventListener('click', ()=>{ if(pubName) selfWatch(pubName); }); }
  document.getElementById('rePairBtn').addEventListener('click', ()=>{ chainReady=false; try{ sso.resetPairing(); }catch(e){} const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display=''; go('onboarding'); beginPairing(); });
  // Onboarding failure affordances: "Try again" re-requests the allowance on the EXISTING
  // session (no resetPairing → reuses the cached grant, no slot churn); "Re-pair from
  // scratch" is the nuclear option that wipes pairing state.
  { const rb=document.getElementById('retryAllowanceBtn'); if(rb) rb.addEventListener('click', async ()=>{ showRetry(false); onboardingStatus('Trying again…'); const ok=await pushChainIdentity(); if(ok){ onboardingStatus('Network access granted ✓'); setTimeout(()=>go('entry'), 700); } }); }
  { const rb2=document.getElementById('rePairBtn2'); if(rb2) rb2.addEventListener('click', ()=>{ chainReady=false; try{ sso.resetPairing(); }catch(e){} showRetry(false); const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display=''; beginPairing(); }); }
  // Settings → "Grant access": (re)request the statement-store allowance on the existing
  // session (triggers the phone popup if not yet granted; a no-op cache hit if it is).
  { const gb=document.getElementById('grantAccessBtn'); if(gb) gb.addEventListener('click', async ()=>{ gb.disabled=true; const prev=gb.textContent; gb.textContent='Requesting…'; try{ await pushChainIdentity(()=>{}); }catch(e){} gb.disabled=false; gb.textContent=prev; updateSettingsStatus(); }); }
  document.getElementById('cancelPublish').addEventListener('click', endPublish);
  document.getElementById('endStream').addEventListener('click', endPublish);
  document.getElementById('startStream').addEventListener('click', goLiveStart);
  document.getElementById('titleForm').addEventListener('submit', (e)=>{ e.preventDefault(); goLiveStart(); }); // Enter in the name field
  document.querySelectorAll('.copy').forEach(btn => btn.addEventListener('click', async ()=>{
    const el = document.getElementById(btn.dataset.copy); const txt = el ? el.textContent : '';
    try { await navigator.clipboard.writeText(txt); } catch(e){}
    const old = btn.textContent; btn.textContent = 'Copied'; btn.classList.add('done');
    setTimeout(()=>{ btn.textContent = old; btn.classList.remove('done'); }, 1200);
  }));

  /* preview-only: dock + mock peer jitter */
  if(!NATIVE){ const dock=document.getElementById('dock'); dock.style.display='flex'; document.querySelectorAll('.dock button').forEach(b=>b.addEventListener('click',()=>go(b.dataset.state)));
    setInterval(()=>{ if(!['live','seed','catchup'].includes(curStateName))return; const base=win.dataset.health==='seed'?6:23; const n=base+((Math.random()*5)|0)-2; renderViewerHealth({peers:n, playing:true, mode:win.dataset.health}); },2200);
  } else { const d=document.getElementById('dock'); if(d) d.remove(); }

  /* QR — real, scannable (the `qrcode` lib). Encodes the SSO pairing payload;
     until host-papp provides the real payload (SSO-2) this renders a placeholder URI. */
  async function renderQr(payload){
    try {
      const svg = await QRCode.toString(payload || 'polkadot://unstation/pair?v=1', {
        type: 'svg', errorCorrectionLevel: 'M', margin: 1,
        color: { dark: '#0B0B0E', light: '#0000' },
      });
      document.getElementById('qrBox').innerHTML = svg;
    } catch(e){ console.error('qr render failed', e); }
  }
  window.__renderPairingQr = renderQr;   // SSO-2 calls this with the live pairing payload
  renderQr();

  /* boot — ambient init + the blur/visibility pause now live in initAmbient() (./ambient.js). */
  // Real Polkadot-app pairing (native only): show the live pairing QR, advance on approval.
  // Shared onboarding status line + the no-reset retry affordance (revealed on a failed
  // allowance so the user can retry WITHOUT resetPairing, which would wipe the cached
  // grant and burn another statement-store slot).
  function onboardingStatus(txt, show=true){
    const el=document.getElementById('pairStatus'), t=document.getElementById('pairStatusText');
    if(el) el.style.display = show ? 'flex' : 'none';
    if(t) t.textContent = txt;
  }
  function showRetry(show){ const r=document.getElementById('allowRetry'); if(r) r.style.display = show ? 'flex' : 'none'; }

  async function beginPairing(){
    const explain = document.querySelector('[data-scene="onboarding"] .qr-copy p');
    const setStatus = onboardingStatus;
    showRetry(false);
    try {
      const res = await sso.signIn(
        payload => renderQr(payload),
        s => {
          console.log('[sso] pairingStatus:', s.step, s);
          if(s.step==='pairing') setStatus('Waiting for you to scan…');
          else if(s.step==='pending') setStatus('Finishing sign-in' + (s.stage ? (' · ' + s.stage) : '') + '…');
          else if(s.step==='finished') setStatus('Linked ✓');
          else if(s.step==='pairingError') setStatus('Error: ' + (s.message || 'pairing failed'));
        }
      );
      console.log('[sso] signIn result:', res);
      if(res && res.ok){
        setStatus('Linked ✓ — one more step on your phone');
        // Let the phone leave its pairing screen before we request the allowance:
        // firing it instantly races the pairing-complete transition and the
        // approval prompt flashes away before it can be acted on.
        await new Promise(r=>setTimeout(r, 1600));
        const ok = await pushChainIdentity(setStatus);
        if(ok){ setStatus('Network access granted ✓'); setTimeout(()=>go('entry'), 800); }
        // On failure pushChainIdentity left an explanatory status; stay on the
        // onboarding screen so the QR + "I've scanned it" remain for a retry.
      }
      else if(explain){ explain.textContent = 'Sign-in didn’t finish' + (res && res.error ? (' (' + res.error + ')') : '') + '. You can Skip for now and try again later.'; }
    } catch(e){ console.error('[sso] beginPairing threw', e); setStatus('Error: ' + ((e && e.message) || e)); }
  }

  // Bridge the paired statement-store allowance to the Rust signer: extract the
  // slot key (decrypted from host-papp's storage) and hand it to the backend so all
  // mesh chain writes are allowance-backed. Without this, presence/signaling are
  // rejected on-chain and nothing connects. Safe to call repeatedly (Rust is idempotent).
  async function pushChainIdentity(onStatus = onboardingStatus){
    if(!(NATIVE && invoke)) return false;
    // Single-flight: boot + ensureSignedIn + the Try-again button can all call this.
    // Firing concurrent ResourceAllocationRequests on one session makes the phone dispose
    // it ("Session disposed"), so coalesce overlapping calls onto one in-flight promise.
    if(_identityInFlight) return _identityInFlight;
    const say = (t)=>{ try{ onStatus && onStatus(t); }catch(e){} };
    _identityInFlight = (async () => {
    try {
      // Provision the statement-store allowance from the phone FIRST. On the initial
      // pairing this prompts for approval on the device; on later launches it's a
      // cached no-op. Only after a grant does the slot-key blob exist to decrypt.
      say('Open your Polkadot app and approve network access — you may need to sign a transaction. This can take a minute; keep the app open.');
      const granted = await sso.requestStatementStoreAllowance();
      if(!granted){
        const reason = (sso.getLastAllowanceError && sso.getLastAllowanceError()) || '';
        console.warn('[chain] statement-store allowance not granted —', reason || 'unknown');
        chainReady = false;
        let msg;
        if(/disposed|Session/i.test(reason)) msg = 'Your link to the phone expired before it finished. Tap Try again — or Re-pair from scratch if it keeps happening.';
        else if(/NotAvailable/i.test(reason)) msg = 'Your Polkadot app couldn’t grant a statement-store slot right now — this account’s per-period allowance is likely temporarily used up. Tap Try again in a minute. (Re-pairing repeatedly consumes more slots and makes it worse.)';
        else if(/Rejected/i.test(reason)) msg = 'The request was declined on your phone. Tap Try again and approve it.';
        else if(/NoSession/i.test(reason)) msg = 'Lost the link to your phone. Re-pair from scratch to retry.';
        else msg = 'Couldn’t set up your network access' + (reason ? ' (' + reason + ')' : '') + '. Tap Try again, or Re-pair from scratch.';
        say(msg);
        showRetry(true);
        return false;
      }
      const key = sso.statementStoreSlotKey();
      if(!key){ console.warn('[chain] allowance granted but slot-key blob missing'); say('Network access granted, but the key didn’t load. Tap Try again.'); showRetry(true); return false; }
      await invoke('set_chain_identity', { slotSecret: Array.from(key) });
      console.log('[chain] paired identity sent to backend');
      // Best-effort: also grab the Bulletin allowance so durable-origin manifest writes
      // are sponsored. Non-fatal — the live stream + mesh work without it; this only
      // restores the on-chain cold-start / late-joiner anchor.
      try {
        if(await sso.requestBulletinAllowance()){
          const bkey = sso.bulletinSlotKey();
          if(bkey){ await invoke('set_bulletin_identity', { slotSecret: Array.from(bkey) }); bulletinReady = true; console.log('[chain] bulletin allowance sent to backend'); }
        }
      } catch(e){ console.warn('[chain] bulletin allowance setup skipped', e); }
      chainReady = true; showRetry(false);
      return true;
    } catch(e){ console.error('[chain] set_chain_identity failed', e); chainReady = false; showRetry(true); return false; }
    })();
    try { return await _identityInFlight; } finally { _identityInFlight = null; }
  }

  async function boot(){
    if(NATIVE){
      document.body.classList.add('native');
      let plat='macos'; try{ plat = await invoke('platform'); }catch(e){}
      document.body.classList.add('plat-'+plat);
      // Windows/Linux use a custom titlebar; wire its min/max/close to the native window.
      if(appWindow){ const wire=(id,fn)=>{ const b=document.getElementById(id); if(b) b.onclick=()=>{ try{ fn(); }catch(e){} }; }; wire('wcMin',()=>appWindow.minimize()); wire('wcMax',()=>appWindow.toggleMaximize()); wire('wcClose',()=>appWindow.close()); }
      if(listen){ try{
        await listen('mesh-stats', e=>applyStats(e.payload));
        await listen('publish-state', e=>applyPublishState(!!(e.payload && e.payload.live)));
        await listen('publish-stats', e=>{ if(e.payload){ lastViewers = e.payload.viewers||0; const el=document.getElementById('pubViewers'); if(el) el.textContent=lastViewers; updatePubHealth(); if(curStateName==='settings') updateSettingsStatus(); } });
        await listen('publish-hint', e=>{ const w=document.getElementById('pubWaiting'); const b=w&&w.querySelector('b'); if(b && e.payload && e.payload.message){ b.textContent=e.payload.message; } });
        await listen('mesh-status', e=>{ const p=e&&e.payload; if(!p) return; console.log('[mesh-status]', p.state, p.detail); chainState=p.state; chainDetail=p.detail||''; updatePubHealth(); if(curStateName==='settings') updateSettingsStatus(); if(p.state==='error'){ const b=document.querySelector('#pubWaiting b'); if(b && !document.getElementById('pubLive').hidden) b.textContent=p.detail; } });
      }catch(e){} }
      // Re-attach: if a publish session is still running in the backend (a webview
      // reload, or relaunch while the process lived), light the Go-Live tab badge so
      // the user can tab straight back into it.
      if(invoke){ try{ const ps = await invoke('publish_status'); if(ps){ pubActive = true; pubName = ps.name; pubHlsUrl = ps.info.hls_url; publishing = true; refreshGoLiveBadge(); } }catch(e){} }
      try {
        // Wait for the saved session to hydrate from storage (async) before
        // deciding — a sync check here races hydration and re-prompts for the QR.
        if(await sso.awaitSession()){
          go('entry');
          // Bridge the allowance to the backend. If it fails (often a stale session the
          // phone already disposed), surface onboarding with a retry instead of sitting
          // on entry looking signed-in while publish/watch silently fail.
          pushChainIdentity().then(ok=>{ if(!ok){ go('onboarding'); const pb=document.getElementById('pairedBtn'); if(pb) pb.style.display='none'; showRetry(true); } });
        }
        else { go('onboarding'); document.getElementById('pairedBtn').style.display='none'; beginPairing(); }
      } catch(e){ go('entry'); }
    } else { document.body.classList.add('plat-macos'); go('entry'); }
  }
  boot();
})();
