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

## Palette — warm sunset on a near-black room

| Token | Hex | Use |
|---|---|---|
| `--ground` | `#120C10` | app background (warm plum-charcoal, video-friendly dark) |
| `--surface` | `#1C141A` | cards / panels |
| `--text` | `#F7EFE9` | primary text (warm off-white) |
| `--dim` | `#BCA9B1` | secondary text |
| `--faint` | `#7E6E78` | tertiary / captions |
| `--coral` | `#FF5C7A` | primary accent |
| `--amber` | `#FFB347` | accent partner |
| `--grad` | `135°, #FF5C7A → #FFB347` | the **signal gradient** — the one primary action per screen, the play-dot, live cues |
| `--success` | `#34D399` | healthy connection |
| `--danger` | `#F2555A` | trouble |

The gradient is for the single action that matters on a screen; everything else stays calm.

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
