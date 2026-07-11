# Frequently asked questions

Plain-language answers. For the deeper technical version, see [Security](security.md).

## Do I need to know anything about crypto or blockchain?

No. You scan a code with an app once, and after that it works like any other video app. Under the
hood it uses a public blockchain as a shared noticeboard, but you never touch that directly, and
you don't need any cryptocurrency or tokens to watch or broadcast.

## Why do I sign in with the Polkadot app?

Two reasons. It proves a real person is behind the account (which keeps bots from flooding the
network), and it hands your device a small, revocable permission slip to post notes to the shared
noticeboard. Your actual keys never leave your phone. The app on your computer only ever holds that
limited permission, and you can revoke it.

## Is it free?

Yes, to watch and to broadcast. The software is free and open source. The only real cost is if you
choose to [run a relay](run-a-relay.md) on a server, which uses bandwidth you pay your hosting
provider for. Watching and broadcasting from the app cost nothing.

## Is it really impossible to shut down?

It's honest to say there is no server and no company in the middle, so there is no operator to
pressure and nothing central to switch off. The video goes straight between people. That removes the
usual single point of failure.

It is not honest to say nothing could ever disrupt it. The shared noticeboard is a public blockchain
that clients reach through network endpoints, and those endpoints are a place pressure could be
applied. Anyone can run their own, and the app lets you point at alternatives, but out of the box
it uses a small set of default endpoints. See [Security](security.md#what-are-the-weak-points) for
the honest list of chokepoints.

## Who can see that I'm watching, or my IP address?

This is peer-to-peer, so anyone you connect to directly can see your IP address, the same way any
video call works. In practice that's the broadcaster or a relay. A broadcaster can hide their own
address behind volunteer relays so ordinary viewers never see it. The notes that set up connections
are always encrypted, so someone just watching the noticeboard cannot harvest addresses. The full,
honest breakdown of what leaks to whom is in [Security](security.md#what-can-each-party-see).

## Can the broadcaster see who is watching?

They can see the connections coming in (again, like any video call), but not a name or an identity,
because there are no accounts. If a broadcaster turns on address hiding, even that is limited to the
relays, not to them.

## Can a relay watch the streams it carries?

For invite-only streams, no. Those are encrypted end to end, so a relay is passing along sealed
video it cannot open. For ordinary public streams, the video isn't encrypted (it's public anyway),
but a relay still can't alter it, because every chunk is fingerprinted and checked.

## What's the difference between unlisted and invite-only?

An **unlisted** stream adds a long random code to its name, so it can't be found by guessing or by
browsing. Anyone with the full link can watch. An **invite-only** stream goes further and encrypts
the video end to end, so only people with the invite link's key can actually see it, and the relays
carrying it can't. Use unlisted for "don't broadcast this widely" and invite-only for "only these
people should ever see it."

## Is watching or broadcasting legal?

Unstation is a tool, like a web browser or a video call app. What you do with it is on you. There is
no moderation team and no company deciding what's allowed, which is the point, but it also means you
are responsible for following the law where you are. Broadcast things you have the right to
broadcast.

## What happens when a lot of people watch at once? Does it get slow?

The opposite of a single server. Because every viewer also helps pass the stream along, a bigger
audience brings more capacity, so popular broadcasts tend to hold up rather than buckle. Volunteer
relays add more headroom on top of that.

## Does it work on my phone, on cell data, or behind my office firewall?

Usually, yes. Direct connections work most of the time, and when two people can't reach each other
directly (common on cell networks and strict firewalls), a volunteer relay bridges them. There's
also an [Android app](https://github.com/lovelaced/unstation-android). The one case that can still
fail is an unusually locked-down network with no relay reachable, where an operator-provided fallback
server would be needed.

## What do I need to broadcast?

The desktop app plus any streaming app (OBS is the common one) pointed at Unstation's local ingest,
or just the phone app and its camera. To broadcast from the desktop you'll also need ffmpeg
installed. Full steps are in the app itself when you choose Go Live.

## Is my data or identity stored anywhere?

There are no accounts and no profile. Your keys stay on your phone. The notes posted to the shared
noticeboard are tied to a per-app pseudonym, not your name, and they expire on their own. The one
honest caveat is that all of one device's activity is signed by the same pseudonym, so someone
watching the blockchain could see that "this pseudonym broadcast these streams," without knowing who
that is. More on that in [Security](security.md#on-chain-metadata).

## What can still go wrong?

It's experimental and unaudited software on a public test network. Connections can fail on hard
networks, the test network has limits, and the security has not been through a formal audit. Treat
it as a capable prototype: great to use and to build on, but measure the limits before you rely on
it for something that really matters.
