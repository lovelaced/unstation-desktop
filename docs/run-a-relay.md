# Run a relay

Relays are the closest thing Unstation has to infrastructure, and anyone can run one. A relay is a
small program on a server somewhere that lends its bandwidth to the network. It helps people who
can't connect to each other directly (a phone on cell data, a viewer behind a strict office
firewall) reach a stream anyway.

A relay never watches the streams it carries. It passes the video along, but every chunk is
fingerprinted and it can't change a single byte, and for invite-only streams the video is encrypted
end to end, so the relay is moving sealed boxes it can't open. Running one is like running a public
phone booth: useful to everyone, revealing nothing.

By default a relay is **open**: it doesn't pick a stream. It announces that it has spare capacity,
and broadcasters who need help ask it to carry their stream. It picks up streams that recruit it,
drops the ones nobody is watching, and splits its bandwidth across whatever it's carrying.

## What you need

A small Linux server (a cheap VPS is plenty) with:

- a **public IP address** and **inbound UDP allowed** (connections use ephemeral UDP ports; there
  are no TCP ports to open, no domain, and no certificates)
- about **1 to 2 CPU cores and 1 GB of RAM**
- **bandwidth**, which is the real cost: roughly the stream's bitrate coming in, and the same again
  for each viewer you serve (about 2.7 GB per hour per viewer at 6 Mbps). Flat-rate providers like
  Hetzner or Netcup are a better fit than clouds that bill for every gigabyte out.

You'll also need the **Polkadot app** on your phone to sign the relay in (see below).

## Install

On the server, as root:

```bash
curl -fsSL https://raw.githubusercontent.com/lovelaced/unstation-desktop/master/scripts/seed/install.sh | sudo bash
```

That installs an open relay. To help specific streams only, add `-s -- --stream <name>` (you can
repeat `--stream`).

The installer downloads the release binary for your machine (and checks it against the release
checksums), creates a locked-down `unstation` system user, and then shows a **QR code**.

## Sign in

A relay needs permission to post its "I'm available" note to the network, and that permission comes
from a real person, the same way the desktop and phone apps get it. When the QR code appears, scan
it with the Polkadot app on your phone and approve the request. That's the whole sign-in.

If you missed the code, or you installed without a terminal, sign in any time:

```bash
sudo -u unstation UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed unstation-node pair
sudo systemctl restart unstation-seed
```

The relay's identity is saved in `/var/lib/unstation-seed`. Keep that directory and you won't have
to scan again, even across upgrades.

## Watch it work

```bash
journalctl -fu unstation-seed
```

You should see it announce its capacity, then join a stream's swarm when one recruits it:

```
[seed] open relay: volunteering 8 stream slot(s) at 50Mbps total
[seed] joining swarm for "friday-night-football" ...
[seed] streams=1/8 (0 pinned) peers_total=3 uplink=3100kbps budget=50Mbps ...
```

Every ten seconds it prints a heartbeat: how many streams it's carrying, how many viewers it's
serving, and how much bandwidth it's using. `chain_write_fail` should stay at `0`.

## Settings

The relay is configured through its service file
(`/etc/systemd/system/unstation-seed.service`). Edit it, then
`sudo systemctl daemon-reload && sudo systemctl restart unstation-seed`.

| Setting | Default | What it does |
|---------|---------|--------------|
| `UNSTATION_NODE_BUDGET_MBPS` | `50` | Total upload to donate, in Mbps, shared across every stream |
| `UNSTATION_NODE_MAX_STREAMS` | `8` | Most streams to carry at once (each needs at least about 4 Mbps) |
| `--stream <name>` | none | Pin a stream to always carry (repeatable); pinned streams are never dropped |

## Good to know

- **One relay per phone.** Your phone hands the relay a signing key derived from your identity, and
  it derives the same one every time, so two relays signed in with the same phone would collide. To
  run several relays, use a different phone for each.
- **Broadcasters who hide their address only accept relays they recruit.** If you *pin* a stream
  whose broadcaster hides their connection, also run in open mode (leave off `--stream`, or add
  `--open`) so that broadcaster can recruit you.
- **If the service shows "failed" and exits with status 78**, it isn't signed in on this network yet.
  Run the `pair` command above and restart. The service deliberately doesn't keep restarting on that
  error, because a restart can't fix a missing sign-in.

## Uninstall

```bash
sudo systemctl disable --now unstation-seed
sudo rm /etc/systemd/system/unstation-seed.service /usr/local/bin/unstation-node
sudo rm -rf /var/lib/unstation-seed
```

## Building the relay yourself

The one-line installer uses a prebuilt binary. To build it from source instead, see
[Contributing and building from source](contributing.md); then install with
`sudo scripts/seed/install.sh --binary path/to/unstation-node`.
