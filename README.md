# Unstation (desktop)

Fully-decentralized P2P live + VOD streaming — desktop publisher + viewer + optional volunteer seed.
No hosted origin, no operator-run seed tier, no operator-run relay.

- Polkadot **statement store** for bootstrap signaling, **Bulletin Chain** as origin-of-record.
- **WebRTC data channels** for segment transfer; an AceStream-style deadline-aware piece-picker (the *mesh* engine).
- The desktop publisher is the **genesis seed**.

Design docs live in the `swarmline/` spec repo (`TECH_SPEC.md`, `IMPLEMENTATION_SPEC.md`).

## Workspace

| Crate | Role |
|-------|------|
| `unstation-core` | the mesh engine — picker, buffer maps, store, protocol, scheduler (IO-agnostic, trait-based) |
| `transport-libdc` | libdatachannel-backed `Transport` |
| `unstation-node` | headless seed + volunteer relay (also embedded for desktop Seed Mode) |
| `desktop/` | Tauri app (Rust + web UI): Watch / Go Live / Seed |

## Build

```bash
cargo build
cargo test            # includes the deterministic simulator + scenarios
cargo bench           # criterion benchmarks
```

> Prototype / proof-of-concept. Reuses the unaudited Parity prototypes (`product-sdk`,
> `polkadot-android-community`) as references; verify live chain limits before any event.
