// User-facing copy, in one place. This is the plain-language layer: chain/mesh
// jargon (statement-store slots, relays, allowances) is translated into words a
// viewer or streamer can act on. Prefer adding strings here over inlining them.
// House style: sentence case, no em dashes, warm and plain. Dropdown/status
// fragments join clauses with ·, prose uses full sentences.

export const STRINGS = {
  // Lending bandwidth (seed-by-default)
  lendPaused: 'Paused. Your connection looked unstable, so sharing is off until it recovers.',
  lendOff: 'Off. Not watching anything right now.',
  // HUD pill mode text ("backup" = segments served from the stream's durable
  // on-chain copy (NodeStats.from_origin), not another viewer).
  modeLiveP2p: 'LIVE · P2P',
  modeLiveBackup: 'LIVE · from backup',

  // "Your connection" drawer — where the video comes from.
  sourcePeers: 'directly from peers',
  sourceBackup: 'from the stream’s backup copy on the network, still verified byte-for-byte',

  // Viewer catchup overlay + stall ladder (a silent "no video" made actionable).
  watchConnecting: 'Connecting to the broadcaster…',
  watchNoVideoFromPeer: 'Connected to a peer, but no video is arriving. The broadcaster may have stopped streaming.',
  watchUnreachable: 'Can’t reach the broadcaster yet. Double-check the stream name and make sure they’re live. Some networks take longer to connect, so hang tight.',
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
  unreachableSub: 'Nobody carrying this stream is reachable yet. We’ll keep trying. Double-check the name and that the broadcaster is live.',
  watchStartFailedEyebrow: 'Problem',
  watchStartFailedTitle: 'Couldn’t start watching',
  // Invite-only stream opened without its key (mesh-status code invite_key_missing):
  // terminal — retrying without the invite link can't help.
  inviteNeededEyebrow: 'Invite needed',
  inviteNeededTitle: 'This stream is invite-only',
  inviteNeededSub: 'The stream name alone can’t unlock it. Ask the broadcaster for their invite link and open it on this device.',
  // A candidate broadcaster failed signature verification (code verify_failed):
  // transient — the search keeps going underneath.
  verifySkippedSub: 'We found a broadcaster we couldn’t verify, so we skipped it. Still looking for the real one.',

  // Player controls.
  tapForSound: '🔊 Tap for sound',
  skipToLive: 'Skip to live',
  behindLive: n => ' · ' + n + 's behind',
  mute: 'Mute',
  unmute: 'Unmute',

  // Settings — the "Backup copy" row (the optional on-chain durable copy).
  backupOn: 'On. Viewers can find the stream even if you briefly drop.',
  backupOff: 'Off · optional backup',

  // Settings — sharing your connection (never the word "seeding"): how much upload
  // Unstation may use to pass streams along to other viewers.
  lendAuto: 'Auto · pauses when your connection is busy',
  lend5: 'Up to 5 Mbps',
  lend2: 'Up to 2 Mbps',
  lend1: 'Up to 1 Mbps',
  lendOffOpt: 'Off · never share upload',
  lendExplain: 'While you watch, Unstation passes the stream along to nearby viewers, and backs off if your connection struggles. Other people in the stream connect directly to your device to receive it; set this to Off if you’d rather not be reachable.',

  // Settings — camera quality (phone broadcasting).
  camQAuto: 'Standard (720p)',
  camQLow: 'Data saver (480p)',
  camQHigh: 'High (1080p)',
  camQExplain: 'Quality of your camera broadcast. Higher looks better but uses more upload and battery. Applies the next time you go live.',

  // Settings — fast connect (broadcaster side).
  fastSetOn: 'On · friends with a fast-connect invite get video straight from you',
  fastSetOff: 'Off · everyone gets the verified stream',
  fastSetExplain: 'Fast-connect invites send your video directly to a few trusted friends, sooner but without the byte-for-byte check. Uses your upload; the verified stream keeps running for everyone.',

  // Settings — volunteer relays (broadcaster side, both platforms).
  helpersOn: 'On · volunteer relays can help carry your stream',
  helpersOff: 'Off · only viewers pass your stream along',
  helpersExplain: 'Volunteer relays are computers people run to give streams more reach. They only ever carry verified (or end-to-end encrypted) video. For unlisted streams, a recruited relay learns the stream exists so it can help, but nothing else.',

  // Settings — the network-pass row when recent chain writes failed (chain_health).
  settingsPassDegraded: 'Granted, but recent network writes failed',

  // Invite links (unstation://watch/<name>) — share bar, QR overlay, entry hint.
  inviteHint: 'Got an invite link? Just open it, or type the stream name here.',
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
  advEncoderStruggling: 'Your encoder is struggling. Lower the bitrate or resolution.',
  advNoViewers: 'No one’s connected yet. Send your invite link.',
  // "Hide my connection" (origin-shield): a shielded stream serves viewers through
  // volunteer relays only, so nobody can watch until one picks the stream up.
  shieldWaitingRelay: 'Hiding your connection. Waiting for a volunteer relay to pick up your stream; viewers can join as soon as one does.',
  // Chain-health poll: network writes failing (or the subscription dropped) while live.
  pubDiscoverWarn: 'Some network announcements are failing, so new viewers may not find your stream. People already watching are fine. Check your network pass in settings.',

  // Encoder setup (desktop-only collapsible panel in the ingest card). The DEFAULT is the
  // modern path (OBS 30+, spoken of as "your streaming app" — WHIP is a wire detail); the
  // classic RTMP setup is one link away for older encoders. Users never pick a protocol.
  obsSetupTitle: 'Set up OBS (classic)',
  obsStep1: 'OBS → Settings → Stream → Service: Custom.',
  obsStep2: 'Paste the Server and Stream key from above.',
  obsStep3: 'Recommended: keyframe interval 1s, CBR, B-frames 0, preset veryfast.',
  obsSetupTitleWhip: 'Set up OBS',
  whipStep1: 'OBS 30 or newer → Settings → Stream → Service: WHIP.',
  whipStep2: 'Paste the URL above as the Server; leave the Bearer token empty.',
  whipStep3: 'Recommended: keyframe interval 1s, CBR, B-frames 0, preset veryfast.',
  whipWaitingHint: 'In OBS (30+), set Service to WHIP and paste the URL above. It goes live on its own.',
  rtmpWaitingHint: 'Point your encoder at the Server + Stream key above. It goes live on its own.',
  ingestSwitchToClassic: 'Older encoder without WHIP? Use the classic setup',
  ingestSwitchToModern: 'On OBS 30+? Switch back to the faster setup',

  // Fast connect — the broadcaster's trust circle. Publisher-direct WebRTC video: sooner
  // than the verified stream, but WITHOUT the byte-for-byte check, so it's offered through
  // a special invite rather than to everyone. The verified stream stays underneath as the
  // automatic fallback.
  fastOff: 'Fast connect',
  fastConnecting: 'Connecting…',
  fastOn: 'Fast connect · on',
  fastBadge: '⚡ direct from the broadcaster · not verified',
  fastUnavailable: 'Fast connect unavailable. Back on the verified stream.',
  fastInviteDesc: 'straight from you, sooner · trusted friends only (not verified)',

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
  if(!reason) return 'We didn’t hear back from your phone. Make sure it’s online and the Polkadot app is open, then tap Try again.';
  if(/disposed|Session/i.test(reason)) return 'Your link to the phone expired before it finished. Tap Try again, or Re-pair from scratch if it keeps happening.';
  if(/NotAvailable/i.test(reason)) return 'Your Polkadot app couldn’t grant a network pass right now. This account’s passes are temporarily used up, so try again in a minute. (Re-pairing repeatedly uses up more passes and makes it worse.)';
  if(/Rejected/i.test(reason)) return 'The request was declined on your phone. Tap Try again and approve it.';
  if(/NoSession/i.test(reason)) return 'Lost the link to your phone. Re-pair from scratch to retry.';
  return 'Couldn’t set up your network access (' + reason + '). Tap Try again, or Re-pair from scratch.';
}
