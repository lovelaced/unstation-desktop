// User-facing copy, in one place. This is the plain-language layer: chain/mesh
// jargon (statement-store slots, relays, allowances) is translated into words a
// viewer or streamer can act on. Prefer adding strings here over inlining them.

export const STRINGS = {
  // Lending bandwidth (seed-by-default)
  lendPaused: 'Paused \u2014 your connection looked unstable, so lending is off until it recovers.',
  lendOff: 'Off \u2014 not watching anything right now.',
  // HUD pill mode text ("helper" = a seed/relay peer passing the video along).
  modeLiveP2p: 'LIVE · P2P',
  modeLiveHelper: 'LIVE · via a helper',

  // "Your connection" drawer — where the video comes from.
  sourcePeers: 'directly from peers',
  sourceHelper: 'through a helper — another viewer passing the video along, still verified byte-for-byte',

  // Viewer catchup overlay + stall ladder (a silent "no video" made actionable).
  watchConnecting: 'Connecting to the broadcaster…',
  watchNoVideoFromPeer: 'Connected to a peer, but no video is arriving — the broadcaster may have stopped streaming.',
  watchUnreachable: 'Can’t reach the broadcaster yet. Double-check the stream name and make sure they’re live. Some networks take longer to connect — hang tight.',
  formatUnsupported: 'This device can’t play the stream format.',
  catchingUp: 'Catching up…',
  tryAgain: 'Try again',
  leave: 'Leave',

  // Finding scene — event-driven phases (`watch-phase` from the backend).
  joiningEyebrow: 'Almost there',
  joiningTitle: 'Joining the mesh',
  joiningSub: 'Connecting to people watching this stream.',
  unreachableEyebrow: 'No luck yet',
  unreachableTitle: 'Can’t reach anyone',
  unreachableSub: 'Nobody carrying this stream is reachable yet. We’ll keep trying — double-check the name and that the broadcaster is live.',

  // Player controls.
  tapForSound: '🔊 Tap for sound',
  skipToLive: 'Skip to live',
  behindLive: n => ' · ' + n + 's behind',
  mute: 'Mute',
  unmute: 'Unmute',

  // Settings — the "Backup copy" row (the optional on-chain durable copy).
  backupOn: 'On — viewers can find the stream even if you briefly drop',
  backupOff: 'Off · optional backup',

  // Invite links (unstation://watch/<name>) — share bar, QR overlay, entry hint.
  inviteHint: 'Got an invite link? Just open it — or type the stream name here.',
  showQr: 'Show QR',
  shareSheet: 'Share…',
  copied: 'Copied',
  close: 'Close',

  // Go-Live preflight checklist (driven by `publish-progress` from the backend).
  stepIdentity: 'Identity',
  stepAnnounced: 'Announced',
  stepEncoder: 'Encoder',
  stepCamera: 'Camera',        // the encoder step's label on mobile (phone camera, no OBS)

  // Publisher health stat cells + advisories (one at a time, priority order).
  statEncoder: 'Encoder',
  statUplink: 'Uplink',
  advEncoderStruggling: 'Your encoder is struggling — lower the bitrate or resolution.',
  advNoViewers: 'No one’s connected yet — send your invite link.',

  // OBS guided setup (desktop-only collapsible panel in the ingest card).
  obsSetupTitle: 'Set up OBS',
  obsStep1: 'OBS → Settings → Stream → Service: Custom.',
  obsStep2: 'Paste the Server and Stream key from above.',
  obsStep3: 'Recommended: keyframe interval 1s, CBR, B-frames 0, preset veryfast.',
  // WHIP ingest (OBS 30+ · lower latency)
  obsSetupTitleWhip: 'Set up OBS (WHIP)',
  whipStep1: 'OBS 30+ → Settings → Stream → Service: WHIP.',
  whipStep2: 'Paste the WHIP URL above as the Server; leave the Bearer token empty.',
  whipStep3: 'Recommended: keyframe interval 1s, CBR, B-frames 0, preset veryfast.',
  whipWaitingHint: 'In OBS 30+, set Service to WHIP and paste the URL above — or run scripts/mock-whip.sh. It goes live on its own.',

  // Fast tier (opt-in, unverified, sub-second WebRTC media, direct from the broadcaster).
  fastOff: 'Low-latency',
  fastConnecting: 'Connecting…',
  fastOn: 'Low-latency · on',
  fastBadge: '⚡ low-latency · unverified · direct',
  fastUnavailable: 'Low-latency unavailable — on the verified stream',

  // Camera-permission recovery (mobile publish).
  camPermHelp: 'Unstation needs camera access to go live. Allow it in Settings, then try again.',
  camStarting: 'Starting your camera…',
  openSettings: 'Open Settings',
};

// Turn a raw pairing/allowance failure (from sso.getLastAllowanceError()) into plain,
// actionable copy. "Network pass" is the user-facing name for the statement-store
// allowance slot the phone grants this device.
export function humanizeError(raw){
  const reason = raw || '';
  if(/disposed|Session/i.test(reason)) return 'Your link to the phone expired before it finished. Tap Try again — or Re-pair from scratch if it keeps happening.';
  if(/NotAvailable/i.test(reason)) return 'Your Polkadot app couldn’t grant a network pass right now — this account’s passes are temporarily used up. Try again in a minute. (Re-pairing repeatedly uses up more passes and makes it worse.)';
  if(/Rejected/i.test(reason)) return 'The request was declined on your phone. Tap Try again and approve it.';
  if(/NoSession/i.test(reason)) return 'Lost the link to your phone. Re-pair from scratch to retry.';
  return 'Couldn’t set up your network access' + (reason ? ' (' + reason + ')' : '') + '. Tap Try again, or Re-pair from scratch.';
}
