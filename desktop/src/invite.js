// Invite links — `unstation://watch/<name>`. makeInviteLink builds the canonical
// form; parseInviteLink accepts sloppy variants (unstation:watch/x, extra slashes,
// uppercase, percent-encoding) and canonicalizes the name through the same slug
// rules the publisher uses (scenes/publish.js shareName), so a link always resolves
// to exactly what a viewer would have typed.

import { shareName } from './scenes/publish.js';

export function makeInviteLink(name){ return 'unstation://watch/' + shareName(name); }

// → the canonical stream name, or null if the URL isn't an invite link.
export function parseInviteLink(url){
  if(typeof url !== 'string') return null;
  const m = /^unstation:\/*watch\/+([^/?#]+)/i.exec(url.trim());
  if(!m) return null;
  let raw = m[1];
  try{ raw = decodeURIComponent(raw); }catch(e){}
  // Guard slugify's 'my-stream' fallback: a name with no alphanumerics at all must
  // not invent a target.
  if(!/[a-zA-Z0-9]/.test(raw)) return null;
  return shareName(raw);
}
