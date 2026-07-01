# Unstation — brand

Friendly, mainstream live-streaming. The pitch is the thing people actually want —
**stream what you want, watch what you want, straight from the source** — and the
peer-to-peer engine just makes it fast and direct. We don't lead with "censorship" or
"can't be taken down"; that's the *how*, not the sell.

## Logo

**Wordmark-forward.** The name is the logo: `Unstation` set in a rounded geometric
weight, with the **"o" replaced by a play-dot** — a circular coral→amber badge holding a
play triangle. Read it and it says *press play*.

- App icon / favicon = just the play-dot in the gradient tile: [`unstation-icon.svg`](unstation-icon.svg).
- Regenerate platform icons (macOS `.icns`, Windows `.ico`, PNGs) from the source:
  ```sh
  cd desktop && pnpm tauri icon ../brand/unstation-icon.svg
  ```

## Palette — warm cinematic on a near-black room

Live tokens (source of truth: `desktop/src/index.html` `:root`). Three warm roles kept
visually distinct: **coral = brand/action**, **amber = seed/fallback**, **green = healthy**.

| Token | Hex | Use |
|---|---|---|
| `--ground` / `--surface-0` | `#0B0B0E` | app background (warm near-black, video-friendly) |
| `--surface-1` | `#16151A` | recessed / rail cards |
| `--surface-2` | `#1E1C24` | raised cards / panels (entry, QR, settings) |
| `--surface-3` | `#28252F` | highest surface |
| `--text` | `#ECE7E4` | primary text (warm off-white) |
| `--dim` | `#9A93A0` | secondary text |
| `--faint` | `#635D69` | tertiary / captions |
| `--brand` | `#FF5C7A` | primary action / brand (coral) |
| `--brand-2` | `#FFB347` | gradient partner (amber) |
| `--brand-grad` | `135°, #FF5C7A → #FFB347` | **signal gradient** — the one primary action per screen, the play-dot, brand moments |
| `--ok` | `#21C9AE` | healthy / direct / verified connection (green) |
| `--seed` | `#FFB347` | seed / fallback / relay state (amber) |
| `--bad` | `#E54B4B` | trouble / error |
| `--info` | `#5AA8FF` | connecting / info |

The gradient and coral are for the single action that matters on a screen; everything
else stays calm. Glass/blur is reserved for floating chrome (titlebar, HUD, panels);
content cards are solid surfaces, elevated by brightness rather than borders.

## Type

- **Display / wordmark** — a rounded geometric face (`ui-rounded` → SF Pro Rounded on
  Apple; falls back gracefully). Used for headlines, buttons, anything with a voice.
- **Body** — system sans (`system-ui`) for reading.
- **Mono** — `ui-monospace` for data/stats (peers, bitrate, hexes).

## Voice

Plain, warm, content-first. Active voice; a control says what it does (“Go live”, not
“Start broadcast session”). Examples:

- Stream what you want. Watch what you want.
- Go live in one click — no account, no setup.
- Your video, straight to the people watching.
- It gets faster the more people watch.

See the full brand guide artifact for the living wordmark, swatches, type scale, and
components in use.
