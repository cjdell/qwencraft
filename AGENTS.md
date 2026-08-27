# AGENTS.md

Guide for AI coding agents working in this repository. Human-facing docs live
in [README.md](README.md); this file is the operational cheat sheet: how to
build, test, verify, and the environment quirks that will bite you.

## What this is

A Minecraft-style voxel engine in Rust that runs in the browser with WebGPU.
An **authoritative server** (infinite seeded world, physics, agents) runs
**two ways**: **embedded in the same wasm module** as the **renderer**
(terrain meshing, voxel lighting + AO, translucent water, block highlight,
first-person controls; direct function calls, no network), or **headless**
(`rustcraft-net`, a tokio WebSocket server — one **shared world** for all
connections: every socket joins the same `Server`, each gets its own
streaming window, and players see each other and each other's edits; `ws://`
/`wss://`). The client renders whatever a `Backend` gives it and forwards
input; it never mutates world state (golden rule 4 holds for both
transports). The wire codec lives in `rustcraft-server::protocol`.

## Golden rules

1. **Everything runs through the Nix shell.** Never call host `rustc`/
   `cargo`/`python3` directly. Prefix every command:
   ```bash
   nix develop --command bash -c '<your command>'
   ```
2. **The root filesystem is 100% full.** Any command that writes temp files
   must set a scratch dir: `env TMPDIR=/home/cjdell/tmp nix develop ...`
   (cargo, chromium, and the verify scripts all honor `TMPDIR`).
3. **Version pins stay in lockstep** — changing one means changing all:
   - `wgpu = "27.0.1"`
   - `wasm-bindgen = "=0.2.100"`, `js-sys = "=0.3.77"`,
     `web-sys = "=0.3.77"`, `wasm-bindgen-futures = "=0.4.45"`
   - `pkgs.wasm-bindgen-cli` in `flake.nix` must match the crate pin
     exactly (mismatch → broken `web/dist` with "unsupported version" at load).
4. **The server is authoritative.** The client renders and forwards input;
   it never mutates world state. All block edits, physics, and spawning are
   server decisions. Keep it that way.
5. **World generation is a pure function of `(seed, world coordinates)`.**
   No stored random state, no `HashMap` of per-column decisions at chunk
   generation time. That's how chunks agree across boundaries (including
   trees, which are stamped from a 1-chunk halo). If you add a terrain
   feature, make it deterministic from coordinates (see `cell_hash` /
   `column_hash` in `rustcraft-world/src/terrain.rs`) and prove it with a
   cross-boundary test.

## Commands

All via the Nix shell (add `env TMPDIR=/home/cjdell/tmp` for anything that
writes temp files):

| Command | What it does |
|---|---|
| `cargo test` | All host unit tests (59: world 22, server 34, net e2e 3 incl. a wss TLS round-trip and a two-client shared-world test). The only place Rust tests run — **wasm tests can't execute here**. |
| `./scripts/build.sh` | Release build → `web/dist` (wasm-bindgen 0.2.100). |
| `./scripts/serve.sh` | Serve `web/dist` at `http://localhost:8080` (python3). |
| `./scripts/serve.sh --https` | Same over TLS with a self-signed cert (`.certs/`, generated once via openssl). **Required for LAN play** — WebGPU needs a secure context. |
| `./scripts/verify.sh` | Headless Chromium smoke test: app start, pointer lock, WebGL2 shadow-render **pixel readback** of the 3D scene, PNG export. This is the main end-to-end check. |
| `./scripts/walk_test.sh` | ~60s scripted walk+fly in headless Chromium; asserts the terrain pool never loses or duplicates blocks (26k+ blocks, compaction safety). |
| `./scripts/npc_test.sh [COUNT] [SPACING]` | Headless NPC load test (`?npcs=COUNT:SPACING`); asserts boot with the load, live count in the HUD, and that steady-state physics runs on the per-agent local block window (hit rate ≥ 99%, solid fallbacks at spawn-tick scale). |
| `./scripts/secure_context_test.sh` | LAN-HTTP (graceful "WebGPU unavailable" message, no panic) + HTTPS startup on localhost and LAN IP. |
| `./scripts/remote_test.sh` | Headless-server e2e: standalone `rustcraft-net` + Chromium with `?server=ws://…`; asserts connect, streamed world, GPU pixel readback of the rendered scene. |

## Architecture map

```
crates/
  rustcraft-world/    PURE, no deps. Blocks, seeded noise, terrain
                      generation (water/trees/snow/flowers/sand), chunk
                      meshing with voxel lighting+AO, raycasting,
                      camera matrices + the WGSL shader source.
                      Host-testable — put geometry/logic tests here.
  rustcraft-server/   Authoritative game state. Server { world, agents,
                      inputs/actions/targets per agent id }, fixed 60Hz
                      tick, physics (walk/jump/fly/swim), per-agent
                      LocalBlockCache (dense 7³ local block window —
                      steady-state physics lookups never touch the chunk
                      buffers; edits invalidate it), NPC load test
                      (Action::Npc*, phyllotaxis spawn), block highlight
                      target, spawn scan. Supports N players (add_player/
                      remove_player/player_ids; `new` = 1 player + ambient
                      NPCs for the builtin, `new_world` = 0 for the net
                      server). Streamer = per-viewer chunk streaming state
                      (sent set + queue + dirty-chunk re-sends) so the
                      builtin (one viewer) and the net server (one per
                      connection) share the logic. protocol.rs = versioned
                      little-endian binary wire codec (ClientMsg/ServerMsg,
                      encode/decode/decode_stream) shared by both
                      transports; pure + host-testable, no deps.
  rustcraft-net/      HEADLESS server binary. tokio + tokio-tungstenite
                      WebSocket front end (ws://, wss:// via --cert/--key).
                      ONE SHARED WORLD: a single Server (Arc<Mutex<
                      WorldState>>) for all connections; each connection
                      registers a player (add_player) and gets its own
                      Streamer (per-viewer chunks). A single 60Hz tick
                      loop ticks the world once and streams/updates every
                      player; block edits re-send to every viewer holding
                      the chunk; each client gets the full Agents list so
                      players see each other. Per-session reader task
                      applies inbound msgs. HOST-ONLY: deps are
                      cfg(not(target_arch = "wasm32"))-gated so the shared
                      workspace wasm build stays green (empty lib + stub
                      bin on wasm). e2e tests (tests/e2e.rs) run real sync
                      WebSocket clients against serve(): single player,
                      two clients sharing one world (see each other +
                      edit sync), and a wss round-trip with an
                      openssl-generated self-signed cert.
  rustcraft-client/   WebGPU renderer. Terrain buffer POOL (one 2M-vertex
                      vbo/ibo, chunks own index ranges, compaction when
                      full), opaque+water pipelines (translucent water
                      pass), agent spheres, wireframe block highlight,
                      clear_terrain() for world switches.
  rustcraft-web/      wasm glue. Backend { Builtin { server, streamer },
                      Remote } — the frame loop talks only to the Backend;
                      builtin drives the embedded Server + Streamer
                      directly, remote decodes ServerMsg frames into the
                      same WorldUpdate/AgentState shapes and forwards
                      input/actions as ClientMsg (so the client renders
                      whatever the world is — builtin or shared). Connect
                      panel (start screen) + ?server=ws://… param; failed
                      connect falls back to builtin. verify_gl.rs = WebGL2
                      "shadow renderer" that re-renders the scene for headless
                      pixel verification.
web/                  index.html (HUD, overlay) + dist/ (build output).
scripts/              build.sh, serve.sh, verify.sh, walk_test.sh,
                      secure_context_test.sh.
```

Data flow per frame: input → `Server::push_action` → (fixed-tick)
`Server::tick` → `player_state()`/delta sync → client uploads changed
chunks to the terrain pool → WebGPU render pass (opaque → water → agents →
highlight). In remote mode the same flow crosses the wire: input →
`ClientMsg` (WebSocket) → the net server ticks its **shared** `Server` once
per 60 Hz step and streams **each** connected player (its own `Streamer`
around that player's position + the full agent list, so players see each
other) → `ServerMsg` frames → each client's `RemoteLink` folds them into
its Backend's state → same render pass. The client's `tick()` is a no-op in
remote mode (the world ticks on the server).

### Invariants that are easy to break

- **Terrain pool capacity** (`VERT_CAP`/`IDX_CAP` in the client) is sized to
  a *measured* worst case (`rustcraft-server/examples/pool_measure.rs`
  measures it — run it after any change that adds vertices per chunk, e.g.
  new mesh features). Compaction drops fog-bound chunks (Chebyshev distance
  ≥ 8 from the player is fully fogged and safe); the walk test guards it.
- **Chunk meshing margin**: `build_chunk_mesh` renders a 5-block margin
  beyond the 16³ chunk so faces at chunk borders agree with neighbors.
  There was a misalignment bug here once (dark scene); regression test
  exists — don't remove it.
- **Actions carry their own aim**: `Action::Break/Place` include the
  click-time yaw/pitch, and the server raycasts block edits with *that*
  aim, not the agent's current one. If you add a new action that raycasts,
  do the same.
- **World edits invalidate agent caches**: `apply_player_action` calls
  `invalidate_caches_at` after every `set_block` — without it an agent
  standing on a broken block would float on the stale cached solid until it
  moved to a new centre cell (the dense window caches air too, unlike the
  old surface-only cache which fell air through to the world). Any new
  world-write path needs the same invalidation. (One-tick staleness after
  an edit is by design: the dirty window rebuilds on the next `update`.)
- **WGSL uniform layout is vec4-only** (112 bytes, no hidden padding).
  Keep it that way or the render silently goes wrong.
- **Water faces are emitted only against air** and rendered in a separate
  translucent pass with no depth writes, after all opaque geometry.
- **`build_chunk` on the client must drop the old pool entry before
  appending** (edit → remesh → duplicate-block bug otherwise).
- **Frame pacing**: an 8ms min-frame guard + rAF *and* 16ms `setInterval`
  drive the loop. Headless Chromium never fires rAF — the interval is what
  keeps the app alive there. Don't "clean up" the interval.
- **WebSocket binaryType**: this headless Chromium reports the socket
  default as `"blob"` (spec says `"arraybuffer"`), so incoming frames
  arrive as Blobs and `Uint8Array(blob)` throws — the connection silently
  delivers *nothing* (onopen fires, onmessage never does). `connect_remote`
  sets `BinaryType::Arraybuffer` explicitly; keep that. Symptom if it
  regresses: "remote socket open" logged but never "remote server
  connected".
- **Virtual time vs cold SwiftShader**: under `--virtual-time-budget`, a
  *cold* WebGPU device init can consume ~20s of virtual time while the
  16ms interval fast-forwards (frames run before the renderer exists); a
  warm init costs ~1s. verify.sh's budget is 40s for exactly this reason —
  don't lower it. Remote mode can't use virtual time at all (a live 60 Hz
  WebSocket never quiesces, so virtual time stalls): `remote_test.sh` runs
  in real time with a wall-clock timeout instead.

## Environment quirks (read before debugging "it's broken")

These are properties of *this machine/headless setup*, not code bugs:

- **Host Vulkan is broken.** lavapipe segfaults here; do not attempt
  host-side GPU rendering or `wgpu` host tests. All GPU verification is
  headless-Chromium WebGPU (which uses SwiftShader internally) or the
  WebGL2 shadow renderer.
- **Headless Chromium cannot composite a WebGPU canvas** into
  `--screenshot` (SharedImage errors) and rejects `buffer.mapAsync` on
  WebGPU surfaces. That's why `verify_gl.rs` exists: it re-renders the
  scene through WebGL2 and reads pixels back. **If you change the 3D
  render path, mirror it in `verify_gl.rs`** or the pixel checks won't see
  it (this bit the block-highlight feature exactly).
- **Headless virtualized time**: `Date.now()` runs ~20× slower than wall
  clock in virtual-time mode, so the app effectively runs at ~3 fps real
  time under `--virtual-time-budget`. Timing-based failures there are
  artifacts; judge logic by state, not frame counts.
- **WebGPU requires a secure context.** On plain-HTTP non-localhost origins
  `navigator.gpu` is undefined and wgpu's `Instance::new` **panics** with a
  misleading "main thread" message. The client checks `navigator.gpu` via
  `js_sys::Reflect::get` before touching wgpu and shows a friendly overlay
  instead — keep that check first.
- **python3 `http.server` has no `--cert`/`--key` flags** here (3.12);
  `serve.sh --https` uses an inline `ssl.SSLContext` +
  `socketserver.ThreadingTCPServer` instead.
- **Nix dev-shell prints a banner before every command** (rustc version,
  command list). When grepping command output, filter with `tail`/`grep`
  or the banner pollutes your checks.
- LAN IP on this machine: `192.168.49.50` (brlan) — used by
  `secure_context_test.sh`; the test discovers the real IP at runtime, so
  this is just for manual testing.

## Gotchas discovered the hard way

- `web-sys` `Navigator::gpu()` needs an unstable cfg flag — use
  `js_sys::Reflect::get(&navigator.into(), &JsValue::from_str("gpu"))`
  instead. `Window::navigator()` returns `Navigator` directly (not
  `Option`); needs the `"Navigator"` web-sys feature.
- `js_sys::JsValue` is **not** re-exported by `js_sys`; import
  `wasm_bindgen::JsValue`.
- `i32::abs_diff` returns **u32**, not i32.
- `#[cfg]` on expression *statements* is unstable — use an `if` or a
  `let _ = x;` sink. `#[cfg]` on `let` statements is fine.
- wgpu 27 blend API: `BlendFactor`/`BlendOperation` (not `Factor`/
  `Operation`); `BlendState::ALPHA_BLENDING` exists, `BlendState::ALPHA`
  does not; `render_pass.draw(v, i)` needs **both** ranges.
- The edit tool (for AI agents) matches `oldText` exactly and multi-edit
  calls are atomic — one bad match fails the whole call.
- `f32`/`f64` mixing requires explicit casts in Rust; physics is `f64`,
  meshing/rendering is `f32` — cast at the boundary deliberately.

## Testing conventions

- Host unit tests live in `#[cfg(test)]` mods in the same file (server) or
  in `crates/rustcraft-world` (geometry/terrain). Tests are
  **seeded-deterministic** (e.g. `Server::new(1337)`) and tick the server
  with fixed `dt = 1/60` — no real time, no `sleep`.
- For physics/spawn tests, don't assume spawn coordinates — the spawn scan
  moves the player to the nearest dry grass column. Read
  `player_state().pos` and work relative to it (the
  `break_and_place` tests aim "down-forward" at `pitch -0.7` for exactly
  this reason).
- New terrain features need: a generation test (it appears), a
  cross-boundary agreement test (two neighboring chunks agree), and, if it
  renders, a mirror in `verify_gl.rs`.
- New mesh features need a `pool_measure` re-run: the worst-case vertex
  count must stay under ~80% of `VERT_CAP`.

## Definition of done (per change)

1. `cargo test` — all green, **zero warnings** (host *and*
   `--target wasm32-unknown-unknown` build: `cargo build --target
   wasm32-unknown-unknown --release`).
2. `./scripts/build.sh` succeeds.
3. `./scripts/verify.sh` — "ALL CHECKS PASSED" (pixel grid stays sane:
   mostly-sky top row, grass/water bottom row).
4. For anything touching the pool, streaming, or world sync:
   `./scripts/walk_test.sh` — "WALK TEST PASSED".
5. For anything touching startup/context: `./scripts/secure_context_test.sh`.
6. For anything touching the protocol, `rustcraft-net`, or the remote
   backend: `./scripts/remote_test.sh` — "ALL CHECKS PASSED".
7. Update README.md if user-visible behavior changed; keep the version
   pins in lockstep if you touched them.
8. Commit on `main` with a message explaining *why* (root cause), not just
   *what*.
