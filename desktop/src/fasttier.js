// Fast tier (viewer, browser-native): the opt-in, unverified, sub-second path.
//
// While watching, the user can flip on "Low-latency": we open a recvonly
// RTCPeerConnection, gather ICE, and hand the offer to Rust (fast_watch_start), which
// relays it to the publisher over the fast-tier signaling topic. The publisher answers
// with a sendonly H.264 track fed straight from its encoder — no HLS, no segmentation,
// no mesh buffering. On `ontrack` we play it in #fastVid, layered on top of the verified
// mesh player, which keeps running warm underneath so fallback is seamless.
//
// This stream is UNVERIFIED (no per-segment hashing) — the badge says so. Anything that
// goes wrong (publisher declines / at capacity / ICE fails / no answer) falls straight
// back to the verified mesh player. Video-only today (the pipeline carries no audio yet).

import { invoke, NATIVE } from './tauri.js';
import { S } from './state.js';
import { STRINGS } from './copy.js';

// Public STUN for srflx candidates so it also works off-LAN; on the same LAN the host
// candidates connect directly and this is never consulted.
const ICE = [{ urls: 'stun:stun.l.google.com:19302' }];
// If no media track arrives this long after the offer goes out, give up and fall back.
// Generous: the offer/answer ride the statement store (seconds each way), then ICE + DTLS
// + the first keyframe. The verified mesh player keeps playing underneath the whole time,
// so a longer wait costs the viewer nothing visible.
const CONNECT_TIMEOUT_MS = 20000;

let pc = null;
let active = false;      // the user has opted in and we're attempting / holding the fast tier
let connected = false;   // a media track is actually playing
let connectTimer = 0;
let badgeTimer = 0;

const byId = (id) => document.getElementById(id);

/** Show the toggle only when this watch is fast-connect ELIGIBLE: the broadcaster shared
 *  a fast-connect invite for this stream (S.fastEligible, set by startWatch). Everyone
 *  else stays on the verified stream with no choice to make. */
export function refreshFastAvailability() {
  const btn = byId('fastBtn');
  if (!btn) return;
  const canFast =
    NATIVE && !!invoke && S.fastEligible && ['live', 'catchup'].includes(S.curState);
  btn.hidden = !canFast;
  if (!canFast && active) disableFastTier(); // left the watch while engaged
}

// Auto-engage once per watch: a friend opening a fast-connect invite shouldn't need to
// find a button — the direct connection is attempted as soon as the verified stream is
// up, and any failure falls back silently to what's already playing.
let autoEngaged = false;
export function maybeAutoEngageFast() {
  if (!S.fastEligible || autoEngaged || active) return;
  autoEngaged = true;
  enableFastTier();
}

export function toggleFastTier() {
  return active ? disableFastTier() : enableFastTier();
}

async function enableFastTier() {
  if (active || !NATIVE || !invoke) return;
  active = true;
  connected = false;
  setBtn('connecting');
  try {
    pc = new RTCPeerConnection({ iceServers: ICE });
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.ontrack = (e) => onTrack(e.streams[0] || new MediaStream([e.track]));
    pc.oniceconnectionstatechange = () => {
      const st = pc && pc.iceConnectionState;
      if (!active) return;
      // Pre-media: any dead state aborts the attempt. Post-media: a broken connection
      // must ALSO fall back — otherwise the fast video silently freezes while the mesh
      // player sits warm underneath. ('disconnected' pre-media is fatal; post-media it
      // can be transient, so only 'failed'/'closed' end an established session.)
      if (!connected && (st === 'failed' || st === 'disconnected' || st === 'closed')) {
        fallback('ice ' + st);
      } else if (connected && (st === 'failed' || st === 'closed')) {
        fallback('ice dropped mid-stream');
      }
    };
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await gatheringComplete(pc);
    await invoke('fast_watch_start', { offerSdp: pc.localDescription.sdp });
    // The answer returns asynchronously via the 'fast-answer' event (applyFastAnswer).
    clearTimeout(connectTimer);
    connectTimer = setTimeout(() => { if (active && !connected) fallback('timeout'); }, CONNECT_TIMEOUT_MS);
  } catch (err) {
    console.error('[fast] enable failed', err);
    fallback('setup');
  }
}

/** Rust delivered the publisher's SDP answer (via the fast-answer event). */
export async function applyFastAnswer(sdp) {
  if (!active || !pc || !sdp) return;
  try {
    console.log('[fast] answer SDP:\n' + sdp);
    await pc.setRemoteDescription({ type: 'answer', sdp });
    console.log('[fast] answer applied; awaiting media');
  } catch (err) {
    console.error('[fast] setRemoteDescription failed: ' + (err && err.name) + ' — ' + (err && err.message));
    fallback('bad-answer');
  }
}

/** Rust says the publisher declined (at capacity) or isn't reachable. */
export function applyFastClosed() {
  if (active) fallback('declined');
}

function onTrack(stream) {
  const fastVid = byId('fastVid');
  const badge = byId('fastBadge');
  if (!fastVid) return;
  connected = true;
  clearTimeout(connectTimer);
  fastVid.srcObject = stream;
  // `.frame video { display:none }` hides it by default — show it like the mesh #vid does.
  fastVid.hidden = false;
  fastVid.style.display = 'block';
  fastVid.play().catch(() => {});
  if (badge) { badge.hidden = false; badge.textContent = STRINGS.fastBadge; }
  setBtn('on');
  console.log('[fast] media playing — sub-second, unverified');
}

/** Tear the fast tier down from outside (watch teardown / leave). */
export function stopFastTier() {
  autoEngaged = false; // the next fast-invited watch auto-engages afresh
  if (active || pc) disableFastTier();
}

async function disableFastTier() {
  const wasEngaged = active;
  active = false;
  connected = false;
  clearTimeout(connectTimer);
  teardownPc();
  const fastVid = byId('fastVid');
  if (fastVid) { fastVid.hidden = true; fastVid.style.display = 'none'; try { fastVid.pause(); } catch (e) {} fastVid.srcObject = null; }
  const badge = byId('fastBadge'); if (badge) badge.hidden = true;
  setBtn('off');
  if (wasEngaged && NATIVE && invoke) { try { await invoke('fast_watch_stop'); } catch (e) {} }
}

/** Any failure → drop the fast tier and let the verified mesh player carry on. */
function fallback(reason) {
  console.warn('[fast] falling back to the verified mesh:', reason);
  disableFastTier();
  const badge = byId('fastBadge');
  if (badge) {
    badge.hidden = false;
    badge.textContent = STRINGS.fastUnavailable;
    clearTimeout(badgeTimer);
    badgeTimer = setTimeout(() => { if (!active) badge.hidden = true; }, 3200);
  }
}

function teardownPc() {
  if (pc) { try { pc.ontrack = null; pc.oniceconnectionstatechange = null; pc.close(); } catch (e) {} pc = null; }
}

function setBtn(stateName) {
  const btn = byId('fastBtn');
  const label = byId('fastLabel');
  if (!btn) return;
  btn.classList.toggle('on', stateName === 'on');
  btn.classList.toggle('pending', stateName === 'connecting');
  btn.setAttribute('aria-pressed', stateName === 'on' ? 'true' : 'false');
  if (label) {
    label.textContent = stateName === 'connecting' ? STRINGS.fastConnecting
      : stateName === 'on' ? STRINGS.fastOn
      : STRINGS.fastOff;
  }
}

// Wait for ICE gathering to finish so the (non-trickle) offer carries its candidates.
// Capped: on a LAN this completes near-instantly; the cap covers a stalled srflx probe.
function gatheringComplete(peer) {
  return new Promise((resolve) => {
    if (peer.iceGatheringState === 'complete') return resolve();
    const done = () => { peer.removeEventListener('icegatheringstatechange', onChange); clearTimeout(t); resolve(); };
    const onChange = () => { if (peer.iceGatheringState === 'complete') done(); };
    const t = setTimeout(done, 2500);
    peer.addEventListener('icegatheringstatechange', onChange);
  });
}

// Wire the HUD toggle once.
{
  const btn = byId('fastBtn');
  if (btn) btn.addEventListener('click', () => toggleFastTier());
}
