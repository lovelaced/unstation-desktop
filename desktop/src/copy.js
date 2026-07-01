// User-facing copy, in one place. This is the plain-language layer: chain/mesh
// jargon (statement-store slots, relays, allowances) is translated into words a
// viewer or streamer can act on. Prefer adding strings here over inlining them.

export const STRINGS = {
  // HUD pill mode text ("helper" = a seed/relay peer passing the video along).
  modeLiveP2p: 'LIVE · P2P',
  modeLiveHelper: 'LIVE · via a helper',

  // "Your connection" drawer — where the video comes from.
  sourcePeers: 'directly from peers',
  sourceHelper: 'through a helper — another viewer passing the video along, still verified byte-for-byte',

  // Viewer catchup overlay + watchdog (a silent "no video" made actionable).
  watchConnecting: 'Connecting to the broadcaster…',
  watchNoVideoFromPeer: 'Connected to a peer, but no video is arriving — the broadcaster may have stopped streaming.',
  watchUnreachable: 'Can’t reach the broadcaster yet. Double-check the stream name and make sure they’re live. Some networks take longer to connect — hang tight.',
  formatUnsupported: 'This device can’t play the stream format.',

  // Settings — the "Backup copy" row (the optional on-chain durable copy).
  backupOn: 'On — viewers can find the stream even if you briefly drop',
  backupOff: 'Off · optional backup',
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
