# RustCraft

A voxel (Minecraft-style) engine written in Rust that runs in the browser.
The world is generated procedurally from a seed, streamed on demand from an
embedded server crate, and rendered with WebGPU: solid-colour blocks with
per-vertex voxel lighting, ambient occlusion and distance fog, agents drawn
as spheres, first-person keyboard + mouse controls.

The landscape includes lakes and shorelines (translucent water you can
swim through), procedural trees with trunks and canopies, sandy beaches,
snow-capped peaks, caves, and flowers scattered on the grassland.

![scene](docs/screenshot.png)

*Real rendered output, 256x144, read back from the GPU during
`./scripts/verify.sh` (the WebGL2 shadow renderer, see below).*

## Quick start

Everything runs through the Nix dev shell (Rust, wasm-bindgen 0.2.100,
chromium, python3, lavapipe):

```sh
nix develop --command bash -c './scripts/build.sh && ./scripts/verify.sh'
```

or, interactively:

```sh
nix develop
./scripts/build.sh     # wasm build -> web/dist
./scripts/serve.sh     # serve on http://localhost:8080
./scripts/serve.sh --https  # HTTPS (self-signed cert) — needed off-localhost
./scripts/verify.sh    # headless-chromium smoke test + pixel checks
./scripts/walk_test.sh # headless walk stress test (terrain pool / streaming)
./scripts/npc_test.sh  # headless NPC load test (physics on cached surfaces)
./scripts/secure_context_test.sh  # secure-context / HTTPS regression test
./scripts/remote_test.sh          # headless-server + browser end-to-end test
cargo test             # host unit tests (worldgen, physics, streaming, …)

# headless server (separate process, one world per connection):
cargo run -p rustcraft-net --release -- --seed 1337 --port 9000
# then open http://localhost:8080/?server=ws://localhost:9000
# — or type the ws:// URL into the connect panel on the start screen.
```

> **Headless server.** `rustcraft-net` runs the authoritative game server
> standalone (tokio, WebSocket). Each connection gets its *own* world (a
> single-player model, one `Server` per socket), ticked at a fixed 60 Hz on
> the server; the browser only renders and forwards input. `?server=`
> (or the connect panel) points the client at it; without it, the embedded
> in-browser server is used exactly as before. See [Headless server](#headless-server-remote-play).

> **Playing from another device on your network:** browsers only enable
> WebGPU in *secure contexts* (`https://` or `localhost`), so
> `http://192.168.x.x:8080` won't work. Run `./scripts/serve.sh --https`
> (generates a self-signed cert in `.certs/` once) and open
> `https://<machine-LAN-IP>:8080` on the other device, accepting the
> browser's certificate warning. The app detects the missing WebGPU on
> plain HTTP and shows an explanatory message instead of crashing.

## Controls

| key | action |
| --- | ------ |
| `W A S D` | move / fly horizontally |
| `Space` / `Shift` | jump / sprint — **up / down while flying** |
| `Mouse` | look (pointer-locked) |
| `Left click` / `Right click` | break / place the highlighted block |
| `Space` (in water) | swim up (falling in water is slowed; hold to surface) |
| `F` | toggle **fly mode** (no gravity, no collision) |
| `Q` / `E` | fly speed down / up (×1.5 steps, 5 → 500 blocks/s; hold to ramp) |
| `N` | spawn the **NPC load test** cloud (replaces existing NPCs) |
| `C` | clear all NPCs |
| `I` / `U` | NPC load count up / down (×2 ÷2, 1 → 2048; hold to ramp) |
| `[` / `]` | NPC spacing down / up (÷2 ×2, 4 → 128 blocks; hold to ramp) |

**NPC load test.** `N` spawns the configured number of wandering NPCs in a
phyllotaxis spiral around you — neighbours sit ~`spacing` blocks apart and
the cloud grows to a radius of ~`spacing × √count`. It exists to load-test
the engine: with hundreds or thousands of agents, the HUD shows the
per-agent **local block window** stats, proving collision physics is served
by the tiny per-agent cache (a 7³ block volume, `window 100%`) instead of
the world's chunk buffers (`solid-fb` stays ~0 — only the spawn tick falls
back). `?npcs=COUNT[:SPACING]` arms the same load on boot for headless
runs (`./scripts/npc_test.sh`); for raw per-tick CPU cost use the host
benchmark `cargo run -p rustcraft-server --release --example bench_tick`.

While flying the HUD shows the current speed (`FLY 120 b/s`). At high
speeds the world streams in around you (terrain is generated on the fly),
so expect the landscape to pop in a few chunks behind the horizon.

**Block highlight.** The block under the crosshair is outlined with a
black wireframe. The server re-computes that target every tick and sends
it with the player state; left/right clicks are applied with the exact
aim from the moment you clicked (the aim is stamped onto the action), so
the highlighted block is always the one that gets broken or built
against — even while you're turning fast.

## Layout

| crate / dir         | what it is                                                              |
| ------------------- | ----------------------------------------------------------------------- |
| `rustcraft-world`   | Block types, seeded noise/terrain, 16³ chunks with 26³ region payloads, chunk meshing (voxel lighting + AO), shared WGSL shader + view-projection math |
| `rustcraft-server`  | The authoritative game server: infinite lazy world (chunks generated on demand), agent simulation (player + NPCs) with a per-agent local block window, fixed-tick physics, delta-based world updates, NPC load test. Plus the wire `protocol` module (binary codec shared by both transports). Runs in-process in the browser *and* inside the headless server |
| `rustcraft-net`     | Headless server: tokio + WebSocket front end over the game server (`ws://`, `wss://` with `--cert`/`--key`); one world per connection, 60 Hz tick loop |
| `rustcraft-client`  | WebGPU (wgpu 27) renderer: shared terrain-mesh buffer pool, sphere agents, fog, first-person camera |
| `rustcraft-web`     | wasm glue: input (keyboard/pointer lock), HUD, main loop, backend abstraction (embedded server or remote over WebSocket) |
| `web/`              | `index.html` page hosting the wasm app                                  |
| `scripts/`          | build / serve / verify / walk-stress / NPC-load / secure-context / remote-server tests |

## Headless server (remote play)

`rustcraft-net` is the standalone server: the same authoritative `Server`
(the browser's embedded server is just this crate running in wasm) wrapped
in a tokio WebSocket front end.

```sh
cargo run -p rustcraft-net --release -- --seed 1337 --port 9000 --bind 0.0.0.0
# TLS for LAN play (WebSockets from an https page need wss://):
cargo run -p rustcraft-net --release -- --cert .certs/cert.pem --key .certs/key.pem
```

- **One world per connection.** Each socket gets a fresh `Server` for the
  configured seed — the current model is single-player; there is no shared
  world or multiplayer sync yet. Disconnecting drops that world.
- **Server is authoritative, as before.** The browser renders server state
  and forwards input (keys, mouse deltas, aim-stamped clicks, the NPC load
  dial); it never mutates world state. The server ticks at a fixed 60 Hz
  independent of the client's frame rate and streams state snapshots; the
  client renders the latest snapshot it holds (at 60 Hz the difference is
  one tick, which reads as smooth).
- **Wire protocol** (`rustcraft-server/src/protocol.rs`): little-endian
  binary frames, versioned (currently 1). Server → client: `Hello`,
  player/agent state, chunk regions, world stats, NPC load echo. Client →
  server: input snapshots, actions (break/place with stamped aim), chunk
  re-send requests (terrain-pool eviction), NPC load changes.

**Connecting the browser:** type the server URL into the panel on the start
screen (or press Enter in the field), or launch with the query param:

```
http://localhost:8080/?server=ws://192.168.49.50:9000
```

`ws://` works from `localhost` pages; from a non-localhost (https) page the
socket must be `wss://`. The HUD's `net` line shows which backend is live
(`builtin (seed …)` or the remote URL), and a failed connection falls back
to the embedded server automatically. `./scripts/remote_test.sh` runs the
whole loop headlessly: standalone server + Chromium in remote mode,
asserting connect, streamed world, and a GPU pixel readback of the
rendered scene.

## NPC load test

The in-game NPC load test (keys above, or `?npcs=COUNT[:SPACING]` / `N` in
the HUD) is a standing stress test for agent physics. Each agent keeps a
dense 7³ **local block window** (343 bytes) around its feet: physics
lookups are answered from the window and only fall back to the world's
chunk buffers for cells outside it. Steady-state probes always stay inside
the window, so the HUD's `window` hit rate should read ~100% and `solid-fb`
(solid reads that still hit the chunk buffers) should stay near 0 —
`npc_test.sh` asserts both, and the host `bench_tick` example reports
per-tick cost per load (player-only ≈ 30µs, 64 NPCs ≈ 80µs,
256 ≈ 200µs, 1024 ≈ 1.4ms on a desktop core — well under the 16.6 ms
60 Hz budget even in wasm; the browser's per-agent sphere rendering is
what eventually saturates first).

`./scripts/npc_test.sh [COUNT] [SPACING] [BUDGET_MS]` (default 500 24) arms
the load in headless Chromium and checks: boot + no JS errors, the live NPC
count in the HUD, `window` hit rate ≥ 99%, and that solid fallbacks stay at
spawn-tick scale (each NPC's first tick, before its window's first build).

## How verification works

`./scripts/verify.sh` serves `web/dist` on a random high port and drives
headless Chromium (SwiftShader WebGL, lavapipe Vulkan for WebGPU):

1. console log must show the startup milestones (app started, renderer
   ready, first frame) and no uncaught JS errors;
2. the HUD DOM must show streamed chunks (server streaming works);
3. **pixel-level**: the app re-renders the exact same scene (same CPU
   meshes, same shared camera math) through a WebGL2 "shadow" renderer —
   headless Chromium cannot composite a WebGPU canvas into screenshots or
   map GPU buffers — and reads the pixels back. verify.sh asserts the 4x3
   region grid shows sky at the top, terrain at the bottom, and a fog
   gradient between;
4. the full shadow frame is streamed back as base64 chunks
   (`VERIFY_PNG i/N …`) and reassembled into `docs/screenshot.png`-style
   PNG output (default: `$TMPDIR/rustcraft-scene.png`).

The WGSL itself is exercised for real: the browser compiles the actual
WebGPU pipeline at startup, and a shader error fails renderer init.

`./scripts/walk_test.sh` drives the app in `?walk=1` mode: the player
holds W (hopping + turning when blocked by 1-step terrain) for ~60 virtual
seconds, walking a few hundred blocks of fresh terrain. It fails if the
terrain buffer pool loses a chunk (the "invisible landscape" bug) or if
frames stop being rendered.

## Terrain buffer pool

All terrain chunk meshes live in one pre-allocated vertex/index buffer
pair (`rustcraft-client`): a frame costs one `set_index_buffer` +
`set_vertex_buffer` plus a single `draw_indexed` per chunk. When the pool
fills, `compact_pool` drops chunks from the *farthest* first (3D Chebyshev
distance, including Y): chunks beyond `FOG_CHUNK_DIST` are fully inside
the fog (nearest corner past `FOG_END` blocks) and are dropped silently;
anything closer that still has to go is reported and the embedded server
re-sends it from its own copy (the server keeps every generated chunk).

Capacity is sized with headroom over the measured worst case: a radius-7
streaming sphere of the current landscape (caves, mountains, lakes, trees,
beaches, snow) needs ~1.0-1.2M vertices in typical walking and ~1.65M at
spawn (`rustcraft-server`'s `pool_measure` example); the pool holds
2M vertices / 3M indices. A walk test with the cap artificially shrunken
to 300K exercises 100+ compactions per minute with zero lost chunks.

## World generation

Everything is a pure function of (seed, world coordinates), so chunks agree
perfectly across boundaries — including tree canopies that overhang a chunk
edge (each chunk stamps the 1-chunk halo around it; enforced by the
`chunk_matches_block_at` and `tree_chunks_agree_across_boundaries` tests):

- **Heightmap** — layered value noise (rolling hills + detail), clamped to
  8..47 blocks; 3D-noise caves carved below the surface.
- **Water** — columns below sea level (y=21) are filled to a flat surface;
  lakebeds are sand, the surface is rendered translucent in a second
  blending pass, and the player (and NPCs) swim: slowed movement, capped
  fall, `Space` to rise.
- **Trees** — one per ~80 flat grass columns: a 4-6 block trunk with a
  5×5 + 3×3 leaf canopy (deterministic corner cutouts), never on beaches,
  snow, or slopes steeper than 1 block.
- **Biomes** — underwater/surface-level columns are sandy beaches, the
  highest columns (y≥33) are snow-capped, the rest are grassland with
  scattered red/yellow flowers (passable decals).

## WebGPU and secure contexts

WebGPU is only exposed by browsers in secure contexts (`https://` or
`localhost`). The renderer checks `navigator.gpu` before touching wgpu (a
missing GPU would otherwise panic deep inside wgpu with a misleading
message) and shows an actionable overlay error instead. `./scripts/serve.sh
--https` serves the app with a self-signed certificate for LAN play; the
headless regression test `./scripts/secure_context_test.sh` covers all three
modes (plain-HTTP-LAN graceful failure, HTTPS localhost, HTTPS LAN).

## Notes / environment quirks

- The host's Vulkan driver may be broken (glibc ABI mismatch). verify.sh
  always forces the lavapipe ICD via `VK_ICD_FILENAMES`; do not rely on
  host-side wgpu/lavapipe tests here.
- Headless Chromium does not fire `requestAnimationFrame`; the app has a
  16 ms `setInterval` fallback driver (with an 8 ms guard against
  double-rendering when both are active).
- The root filesystem can be full; run with
  `env TMPDIR=/home/cjdell/tmp` so build/chromium scratch goes to `/home`.
