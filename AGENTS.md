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
(`qwencraft-net`, a tokio WebSocket server — one **shared world** for all
connections: every socket joins the same `Server`, each gets its own
streaming window, and players see each other and each other's edits; `ws://`
/`wss://`). The client renders whatever a `Backend` gives it and forwards
input; it never mutates world state (golden rule 4 holds for both
transports). The wire codec lives in `qwencraft-server::protocol`.

`qwencraft-net` additionally runs a small **dashboard** on the *same*
port, under `/dashboard/`: a dioxus (wasm) app — player/NPC counts, an
event log, and a pan/zoom 2D minimap of the world with agents plotted —
served from `dashboard/dist`, which is embedded into the server binary
via `include_dir!` (the built assets are committed). The same port hosts
the WebSocket at `/ws`, the dashboard under `/dashboard/`, the game page
at `/`, and `/api/*` + `/healthz` — one port, one authority. The map is
computed off the tick path from the pure terrain function + the world's
edit history, never touching the authoritative world's lock.

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
   - The dashboard workspace (`dashboard/Cargo.toml`) pins the same
     `wasm-bindgen =0.2.100` / `js-sys` / `web-sys` (its
     `wasm-bindgen-futures` is `=0.4.50` — the release that pairs with
     0.2.100/0.3.77).
   - `pkgs.wasm-bindgen-cli` in `flake.nix` must match the crate pin
     exactly (mismatch → broken `web/dist` with "unsupported version" at
     load). `scripts/build_dashboard.sh` asserts CLI == lockfile version.
4. **The server is authoritative.** The client renders and forwards input;
   it never mutates world state. All block edits, physics, and spawning are
   server decisions. Keep it that way.
5. **World generation is a pure function of `(seed, world coordinates)`.**
   No stored random state, no `HashMap` of per-column decisions at chunk
   generation time. That's how chunks agree across boundaries (including
   trees, which are stamped from a 1-chunk halo). If you add a terrain
   feature, make it deterministic from coordinates (see `cell_hash` /
   `column_hash` in `qwencraft-world/src/terrain.rs`) and prove it with a
   cross-boundary test.

## Commands

All via the Nix shell (add `env TMPDIR=/home/cjdell/tmp` for anything that
writes temp files):

| Command | What it does |
|---|---|
| `cargo test` | All host unit tests (96: world 45 incl. the terrain-pool allocator tests and the face-UV-span test, client 2 — naga validation of the WGSL shader module + texture-function census, server 40 incl. the resync repair test, net e2e 5 incl. a wss TLS round-trip, a two-client shared-world test, a dashboard HTTP test, and a resync-over-a-real-socket test, net lib 4 — the map). The only place Rust tests run — **wasm tests can't execute here** (but the WGSL *is* checked here, see the client tests). |
| `./scripts/build.sh` | Release build → `web/dist` (wasm-bindgen 0.2.100). |
| `./scripts/serve.sh` | Serve `web/dist` at `http://localhost:8080` (python3). |
| `./scripts/serve.sh --https` | Same over TLS with a self-signed cert (`.certs/`, generated once via openssl). **Required for LAN play** — WebGPU needs a secure context. |
| `./scripts/verify.sh` | Headless Chromium smoke test: app start, pointer lock, WebGL2 shadow-render **pixel readback** of the 3D scene, PNG export. This is the main end-to-end check. |
| `./scripts/walk_test.sh` | ~80s scripted walk→fly→return-walk in headless Chromium (`SEED=N` env, default 1337); the fly phase makes a long corridor so the pool evicts the walk endpoint as fog-bound trail, then the player walks back through that re-entered terrain. Asserts no *sustained* visible holes (3+ consecutive POOL samples > 15 missing meshed-but-evicted chunks) — catches a broken eviction→re-stream path; a single-sample transient (fast re-entry) is OK. |
| `./scripts/npc_test.sh [COUNT] [SPACING]` | Headless NPC load test (`?npcs=COUNT:SPACING`); asserts boot with the load, live count in the HUD, and that steady-state physics runs on the per-agent local block window (hit rate ≥ 99%, solid fallbacks at spawn-tick scale). |
| `./scripts/secure_context_test.sh` | LAN-HTTP (graceful "WebGPU unavailable" message, no panic) + HTTPS startup on localhost and LAN IP. |
| `./scripts/remote_test.sh` | Headless-server e2e: standalone `qwencraft-net` (single port, `?server=ws://…/ws`) + **two** Chromium browsers in the same shared world; asserts both connect, the server sees both, the first browser renders both players (`POOL … agents=2`), streamed world, GPU pixel readback. |
| `./scripts/wan_resync_test.sh [RTT_MS]` | Deterministic TRANSIT-LOSS test: headless Chromium → `wan_proxy.py` (TLS end, 200 ms RTT, drops ~4 MB of **whole WS frames** from the middle of the initial chunk burst) → `qwencraft-net`; asserts the drop happens, the client detects the gap and requests a resync, the server re-sends exactly the missing regions, and the view recovers (final `POOL chunks=` ≥ 250). Real-time (no virtual time — the resync timers use wall-clock `Date.now()` and a live 60 Hz socket never quiesces). |
| `./scripts/build_dashboard.sh` | Builds the dashboard (its own wasm workspace) → `dashboard/dist` (wasm-bindgen + html/css). The dist is **embedded into the `qwencraft-net` binary and committed** — after running it, rebuild `qwencraft-net` and commit `dashboard/dist`. |
| `./scripts/dashboard_test.sh` | Dashboard e2e: single-port curl checks (`/healthz`, `/api/status`, `/api/map`, `/dashboard/*` assets, game at `/`, 426 on `/ws` over plain HTTP, 404) + headless Chromium on `/dashboard/` (DOM shows the live server, screenshot shows the rendered minimap). |

## Architecture map

```
crates/
  qwencraft-world/    PURE, no deps. The BLOCK REGISTRY (block.rs —
                      single source of truth for all 17 block types:
                      Block enum + BLOCKS const with physics, face
                      texture ids, CPU colours, placeability, PLACEABLE),
                      seeded noise, terrain generation (water/trees/snow/
                      flowers/sand), chunk meshing with voxel
                      lighting+AO (vertices carry [pos, light, uv, texId]),
                      raycasting, camera matrices + view-projection math
                      (uniform_bytes takes `time` for the water ripples),
                      the minimap's per-column top-block queries
                      (column_top, sharing top_block/sub_top_block with
                      the generator), and Vec3 (shared math type). The
                      WGSL shader + textures live in the client. Host-
                      testable — put geometry/logic tests here.
  qwencraft-server/   Authoritative game state. Server { world, agents,
                      inputs/actions/targets per agent id }, fixed 60Hz
                      tick, physics (walk/jump/fly/swim), per-agent
                      LocalBlockCache (dense 7³ local block window —
                      steady-state physics lookups never touch the chunk
                      buffers; edits invalidate it), NPC load test
                      (Action::Npc*, phyllotaxis spawn), block highlight
                      target, spawn scan. Supports N players (add_player/
                      remove_player/player_ids; `new` = 1 player + ambient
                      NPCs for the builtin, `new_world` = 0 for the net
                      server). Agents carry a display name + colour
                      (players choose both; `set_profile` sanitises the
                      name — trim, strip control chars, 24-char cap).
                      Streamer = per-viewer chunk streaming state (sent
                      set + queue + dirty-chunk re-sends) so the builtin
                      (one viewer) and the net server (one per
                      connection) share the logic. protocol.rs = versioned
                      little-endian binary wire codec (ClientMsg/ServerMsg,
                      encode/decode/decode_stream, currently v6: Hello
                      carries the player id, agents carry name + colour,
                      ClientMsg::Profile sends the player's name/colour,
                      Action::Place carries the selected block id (u8 —
                      the server validates it via
                      Block::from_u8(...).is_placeable() and silently
                      drops unknown ids), ClientMsg::Resync carries the
                      client's complete chunk set for transit-loss repair
                      — the server re-sends every ready chunk in view the
                      client doesn't have (the streamer's `sent` set can't
                      know a chunk was lost before the client ever saw it
                      — eviction reports only cover chunks once held),
                      and the browser console API (window.qwc): GetBlock/
                      BlockAt (authoritative reads — answered from the
                      world, never from the client's own streamed copy),
                      SetBlock and Teleport (applied via
                      Server::console_edit_block / console_teleport —
                      the same world-write path as player edits, but the
                      whole registry is accepted, not just is_placeable)
                      shared by both transports; pure + host-testable, no
                      deps.
  qwencraft-net/      HEADLESS server binary. tokio + tokio-tungstenite.
                      SINGLE PORT (one authority): dispatch_conn sniffs
                      the first bytes — TLS ClientHello → accept TLS
                      first; otherwise the request head is pre-read
                      (10s timeout, 8KB cap) and replayed through a
                      `Prepended<T>` adapter (replay buffer + proxied
                      AsyncWrite — without the replay, consuming the head
                      hangs tokio-tungstenite's accept_async). route:
                      `/ws` + WebSocket-Upgrade header → the socket;
                      `/ws` over plain HTTP → 426; `/dashboard` → 302 to
                      `/dashboard/`; `/dashboard/*` → the embedded
                      dashboard/dist; `/` + `/*` → the embedded
                      web/dist game page (build.rs materialises a
                      placeholder index.html in gitignored web/dist when
                      it's absent); `/healthz`, `/api/status` (JSON,
                      agents include name), `/api/map?x=&z=&w=&h=`
                      (binary 2-bytes/column). ONE SHARED WORLD: a single
                      Server (Arc<Mutex<WorldState>>) for all
                      connections; each connection registers a player
                      (add_player) and gets its own Streamer (per-viewer
                      chunks). A single 60Hz tick loop ticks the world
                      once and streams/updates every player; block edits
                      re-send to every viewer holding the chunk; each
                      client gets the full Agents list (named, coloured)
                      so players see each other. Per-session reader task
                      applies inbound msgs. map.rs = MapState (own lock,
                      own WorldGen + height cache + last-wins edit
                      overlay — never touches the world lock) computing
                      the topmost block per column from pure terrain +
                      the world's edit history (exact modulo
                      flowers/canopy overhang — see its tests). `--debug`
                      logs per-second per-player streaming telemetry to
                      stderr (sent/queue/pos — the tool that pinpoints
                      "server sent it, client never got it"). The
                      Server's event_sink (set by serve()) feeds the
                      EventLog (own mutex, cap 256 — must stay outside
                      the world lock). examples/ws_probe.rs = tiny manual
                      protocol probe (connect, decode Hello). HOST-ONLY:
                      deps are cfg(not(target_arch = "wasm32"))-gated so
                      the shared workspace wasm build stays green (empty
                      lib + stub bin on wasm). e2e tests (tests/e2e.rs)
                      run real sync WebSocket clients against serve():
                      single player (incl. Profile name/colour broadcast),
                      two clients sharing one world (see each other +
                      edit sync), a wss round-trip with an
                      openssl-generated self-signed cert, the
                      dashboard HTTP endpoints (single port, /dashboard/),
                      and resync (a client reporting a partial chunk set
                      gets exactly the missing regions re-sent).
  qwencraft-client/   WebGPU renderer. Terrain buffer POOL (one 2M-vertex
                      vbo/ibo, chunks own index ranges, compaction when
                      full), opaque+water pipelines (translucent water +
                      glass pass, per-texture alpha), the WGSL shader
                      (shader.wgsl) + PROCEDURAL BLOCK TEXTURES
                      (textures.wgsl — one WGSL function per TEX_* id,
                      sampled in the fragment stage; shader.rs/
                      textures.rs just embed the files via include_str!
                      and hold the module docs), agent spheres
                      (sphere.rs), wireframe block highlight,
                      clear_terrain() for world switches. The crate only
                      compiles for wasm32, BUT tests/wgsl_valid.rs is a
                      host-runnable integration test (naga dev-dep) that
                      type-checks the concatenated WGSL module + censuses
                      the texture functions — the fast gate for shader
                      edits (see the WGSL gotchas below).
  qwencraft-web/      wasm glue. Backend { Builtin { server, streamer },
                      Remote } — the frame loop talks only to the Backend;
                      builtin drives the embedded Server + Streamer
                      directly, remote decodes ServerMsg frames into the
                      same WorldUpdate/AgentState shapes and forwards
                      input/actions as ClientMsg (so the client renders
                      whatever the world is — builtin or shared).
                      Start screen: big "CLICK ANYWHERE TO PLAY" +
                      Options panel (player name + colour palette, server
                      URL + Connect, disconnect→builtin); clicking inside
                      the panel never triggers pointer lock. RemoteLink
                      sends ClientMsg::Profile on Hello (name/colour from
                      the options state) and skips the player's OWN id
                      when building the agent list (the local player is
                      the camera; other players are rendered as spheres
                      with floating DOM name tags — #tags container,
                      projected with the same view-projection). RemoteLink
                      keeps `have` (every chunk pos ever received,
                      de-duped, trimmed to view+2 past 8192) and
                      reconciles against the server's per-viewer
                      `chunks_sent` (Stats): gap > 32 chunks with no
                      chunk arriving for 5 s (10 s cooldown) sends
                      ClientMsg::Resync(have) — the transit-loss repair
                      (a healthy-but-slow link keeps chunks arriving, so
                      it can't false-positive; a spurious resync is a
                      server-side no-op). ?server=
                      accepts a bare host[:port] (normalise_ws_url
                      appends /ws); failed connect falls back to builtin.
                      ?dbg=1 logs a verbose chunk-receive/eviction trace
                      (WAN debugging).
                      HOTBAR: 9-slot strip over the first 9 of the 13
                      PLACEABLE blocks (DOM, built from the registry),
                      digits 1–9 + mouse wheel select (wheel ignored in
                      text inputs), right-click places the selected block.
                      CONSOLE API (window.qwc): getBlock (Promise —
                      builtin answers synchronously, remote round-trips
                      GetBlock→BlockAt via pending_blocks, FIFO per pos,
                      rejected when the link dies), setBlock (name or id;
                      whole registry, server-side validation), getPlayer
                      (sync from the latest state), setPlayerPos (teleport),
                      listBlocks, help; a usage greeting is logged on
                      startup (install_console_api). Promises are made
                      with js_sys::Promise::new — its executor runs
                      synchronously and hands over the resolve/reject
                      functions (stashed in JsPromise). Closures become variadic JS functions
                      via to_variadic_js: a bare Closure::into_js_value()
                      exposes the RAW wasm adapter, which expects the
                      Vec<JsValue> arguments as a single array —
                      qwc.getBlock(1,2,3) from JS would pass undefined
                      and crash in passArrayJsValueToWasm. (wasm-bindgen
                      0.2.100 removed JsValue::from_fn — into_js_value is
                      the current way to expose a Closure as a JS
                      function.)
                      verify_gl.rs = WebGL2 "shadow renderer" that
                      re-renders the scene for headless pixel
                      verification — behind the `verify` cargo feature
                      (scripts/build.sh enables it; drop it for a
                      production build). Its GLSL texture library must
                      stay a MECHANICAL translation of textures.wgsl
                      (same function per TEX_* id; see the WGSL/GLSL
                      portability notes below).
web/                  index.html (HUD, overlay) + dist/ (build output).
dashboard/            STANDALONE cargo workspace (its dep graph must not
                      touch the main workspace's exact pins). Dioxus 0.7
                      wasm app: top bar (players/npcs/seed/uptime), event
                      log, PLAYERS list (focus button), 2D minimap canvas
                      (drag-pan, wheel-zoom 50–800%; terrain colours
                      mirror Block::color_top; the pane is a mosaic of
                      cached 256×256-block tiles fetched from
                      /api/map — max zoom-out fills the pane; ?zoom=N
                      sets the initial zoom). Built by
                      scripts/build_dashboard.sh into dist/ (COMMITTED —
                      embedded into the qwencraft-net binary via
                      include_dir!; the cdylib exposes #[wasm_bindgen]
                      start(), called from index.html after init()).
scripts/              build.sh, serve.sh, verify.sh, walk_test.sh,
                      secure_context_test.sh, remote_test.sh,
                      wan_proxy.py (TLS-terminating WAN emulator: burst
                      RTT + rate limit + optional whole-WS-frame drops;
                      one thread per connection, no asyncio),
                      wan_resync_test.sh (deterministic transit-loss
                      test — see DoD item 6b),
                      build_dashboard.sh, dashboard_test.sh.
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

Dashboard flow (independent of the game frame loop): the dioxus app polls
`GET /api/status` (1 s: agents + event log) and `GET /api/map` (on view
change + a 3 s refresh: 2 bytes/column, row-major) over same-origin HTTP;
the map endpoint answers from `MapState` (own lock), which the tick loop
keeps in sync with the world's append-only edit history — so a block edit
is visible on the map within one tick.

### Invariants that are easy to break

- **Terrain pool capacity** lives in `qwencraft_world` as
  `TERRAIN_POOL_VERTS`/`TERRAIN_POOL_IDX`; the client aliases them as
  `VERT_CAP`/`IDX_CAP`. **Do not fork the numbers in the client** — a stale
  2M/3M fork (while the world crate said 2.5M/3.75M) left the worst-case
  view at 93.5% of the *real* pool, so compaction dropped still-visible
  chunks (holes that only filled on a block edit). Sized to a *measured*
  worst case: `qwencraft-server/examples/pool_measure.rs` scans seeds and
  the `worst_view_fits_terrain_pool_with_headroom` host test pins the known
  worst positions — both fail if the worst view exceeds 80% of the caps.
  Run `pool_measure` after any change that adds vertices per chunk (new
  mesh/terrain features) and update the pins. Under pressure the pool evicts
  fog-bound chunks first (Chebyshev distance ≥ 8 from the player is fully
  fogged and safe) and reuses the dropped slot in place via the
  `qwencraft_world::pool` free-list allocator — no full-pool re-upload
  (that was the fly-mode stutter); the walk test guards the
  eviction→re-stream path end to end.
- **Chunk meshing margin**: `build_chunk_mesh` renders a 5-block margin
  beyond the 16³ chunk so faces at chunk borders agree with neighbors.
  There was a misalignment bug here once (dark scene); regression test
  exists — don't remove it.
- **Every face quad spans both UV axes** (enforced by
  `every_face_quad_spans_both_uv_axes` in mesh.rs): top/bottom use
  u=x, v=z; sides use u = the face's *horizontal world axis* (z on ±X
  faces, **x** on ±Z faces) and v = y (1 = top edge). There was a bug
  where all sides used u=z: on ±Z faces z is the face's normal, so u
  was constant and every north/south face rendered as a 1D texture in v
  (flat horizontal bands) while the east/west faces looked fine. If you
  touch the corner/UV code, keep that test green. (Related: every
  `tex_*` function must vary in BOTH uv directions — a function of
  uv.x alone is a 1D texture in disguise; see the contract in
  `qwencraft-client/src/textures.rs`.)
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
- **WGSL↔GLSL texture portability.** The procedural textures must exist
  in two dialects: `crates/qwencraft-client/src/textures.wgsl` (the real
  renderer) and the GLSL `TEX_LIB` in
  `crates/qwencraft-web/src/verify_gl.rs` (the headless pixel check) as a
  *mechanical translation* — same function per `TEX_*` id, same math.
  The shared subset is fract/mix/smoothstep/step/sin/cos/atan/length/dot/
  floor/abs, and every WGSL/GLSL divergence goes through a shared helper:
  `tex_atan2` (WGSL has NO two-arg `atan(y,x)` — Dawn rejects it even
  though naga accepts it) and `tex_fmod` (WGSL's float remainder is `%`,
  GLSL ES 3.00's `%` is integer-only — its float remainder is `mod()`).
  Other hard rules: WGSL `clamp` has **no scalar broadcasting** (bounds
  must be `vec3<f32>` when the value is a vec3), bare `0.0`/`1.0`
  literals are *abstract-float* and can't mix with concrete f32 args in a
  call (write `0.0f`), and `smoothstep` edges must be **ascending**
  (descending edges are undefined in WGSL — write
  `1.0 - smoothstep(e1, e0, x)`, pixel-identical). The GLSL side must
  compile under GLSL ES 3.00 (WebGL2) — no desktop-only builtins.
  `cargo test` (the client's naga test) catches WGSL-side breakage
  instantly; the GLSL side is caught by verify.sh's shadow renderer
  (a GLSL compile error there shows up as "VERIFY_PIXELS gl context
  unavailable").
- **New texture ids need both sides + the dispatch.** Adding a `TEX_*`
  id means: the WGSL function + a threshold branch in `sample_tex`
  (and `tex_trans_alpha` if translucent), the GLSL mirror + the same
  branches, and the `every_texture_function_is_defined` test's list if
  the id maps to a new function. The dispatch is threshold-based
  (`tex < 0.5`, `tex < 1.5`, …), NOT `tex == N.0` — the id travels as a
  smoothly-interpolated varying and drifts ~1 ulp, so equality misses a
  fraction of every face (this produced debug-magenta pixels once).
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
  in real time with a wall-clock timeout instead. (The dashboard has no
  live WebSocket — plain fetch polling — so it *can* use virtual time.)
- **The dashboard dist is embedded and committed**: `qwencraft-net` compiles
  `dashboard/dist` into the binary via `include_dir!` — the server has no
  filesystem dependencies at runtime. After changing anything under
  `dashboard/` (sources, html, css): `./scripts/build_dashboard.sh`,
  `cargo build -p qwencraft-net`, and **commit the new `dashboard/dist`**
  (stale dist → stale dashboard on every machine). The JS imports a
  `snippets/` subtree, so the *whole* tree must be served (the include_dir
  approach exists for that; don't regress to per-file include_bytes).
- **Dashboard locks**: `MapState` and `EventLog` each own a mutex and must
  never be guarded by (or lock while holding) the `WorldState` mutex — the
  tick loop holds the world lock while syncing map edits and pushing
  events. The map also must never read the authoritative world (it has its
  own `WorldGen` + edit overlay; that's how it stays off the tick path).

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

- **Dawn reports ONE shader error per module** and wgpu's naga (in-wasm)
  is more lenient than Dawn in at least one documented case (two-arg
  `atan`), so an invalid WGSL module can sail through `Renderer::new`
  ("renderer ready" is logged) and only surface later as console
  "Error while parsing WGSL" + "Invalid ShaderModule" — while the app
  keeps "rendering" (headless can't composite the WebGPU canvas, and
  verify.sh's pixels come from the GLSL shadow path, so it stays green
  with a broken WebGPU shader). Grep the chrome logs for `WGSL` when in
  doubt; fix shader bugs against `cargo test` (naga) first, then the
  browser. (This is why verify.sh passing ≠ the WGSL is valid.)
- **remote_test.sh must not reuse a stale server binary.** The binary
  EMBEDS web/dist + dashboard/dist via `include_dir!`, which cargo does
  not fingerprint — a binary built before the latest web/dist serves a
  stale game page *and* (worse) a stale wire protocol: a v3 server + v4
  client just logs "server speaks protocol 3, client has 4 — closing"
  and falls back to builtin, so the whole remote scenario under test
  never runs while most checks still look plausible. The script now
  detects any file in web/dist or dashboard/dist newer than the binary,
  touches `crates/qwencraft-net/src/lib.rs`, and rebuilds. (Same reason
  as the dashboard dist rule below.)
- **remote_test.sh starts the browsers SEQUENTIALLY** (first browser
  connects, then the second starts): newcomers spawn 1.6 blocks in front
  of the existing player's view (`Server::add_player`), so ONLY the first
  player can see the other's name tag — the newcomer's tag is behind its
  camera (`project_point` → `None` → hidden) and never produces a TAGS
  telemetry line. Letting both race to connect made the tag-position
  check order-dependent and flaky.
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
- The visible-hole metric (client `missing_visible`, the `POOL` telemetry
  line) counts **`known`-but-not-in-pool** chunks, where `known` = chunks
  whose mesh was **non-empty**. Do *not* switch it to the streamer's `sent`
  set: buried (geometry-less) chunks are sent but never meshed, so a
  sent-based count reads ~200+ "missing" on a perfectly healthy view.
  `known`-based reads 0. (This distinction is why the walk test asserts
  *sustained* missing, not a single high sample.)
- **Transit-loss reconciliation (protocol v5 `Resync`)**: the streamer's
  `sent` set marks chunks *queued*, not *received* — a burst lost in
  flight before the client ever saw it is invisible to it (eviction
  reports only cover chunks the client once held), so the spawn view can
  stay a permanent hole on a flaky link until a block edit forces a
  dirty re-send. The fix is a client→server reconciliation: the client
  tracks `have` (distinct chunk positions received) and, when the
  server's per-viewer `chunks_sent` (Stats) runs > 32 ahead with no chunk
  arriving for 5 s (10 s cooldown), sends `ClientMsg::Resync(have)`;
  `Streamer::resync` re-queues every ready chunk in the view radius not
  in `have` — uncapped (repair path, usually a small set), same window
  and readiness checks as the normal send pass. A healthy-but-slow link
  can't false-positive (chunks keep arriving → `last_chunk_ms` stays
  fresh), and a spurious resync is a no-op (the server only re-queues
  what the client lacks). The `POOL` line's `sent=` field is this
  server-side count — `sent - chunks` beyond a few in-flight regions is
  the signature of this bug. `scripts/wan_resync_test.sh` proves the
  whole loop over a real TLS socket with deterministic frame drops.
- `std::time::Instant` **panics on wasm32** ("time not implemented on this
  platform") — the dashboard (and any wasm code) must time things with
  `js_sys::Date::now()` (f64 ms). A panic inside a spawned async task
  cascades into `RefCell already borrowed` panics in wasm-bindgen-futures'
  global queue, which hides the root cause — look for the *first* panic.
- A dioxus wasm app must be a **cdylib with an explicit `#[wasm_bindgen]`
  entry** (`start()`), not a binary: the wasm-bindgen glue only re-exports
  `#[wasm_bindgen]` functions, and a binary's `main` (argc/argv) is not one.
  Also note cargo names the cdylib artifact with **underscores**
  (`qwencraft_dashboard.wasm`) — a stale hyphenated *bin* artifact left in
  `target/` will silently ship the wrong wasm (symptom: "does not provide
  an export named 'start'"). `scripts/build_dashboard.sh` pins the
  underscored path.
- Dioxus 0.7 + wasm-bindgen 0.2.100 API notes: `Signal::write()` needs a
  **mut** signal binding; `Signal::read().clone()` clones the *handle*
  wrapper — clone the inner value with `(*sig.read()).clone()`; `JsValue`
  has no `Display` (use `e.as_string()` / `{:?}`); `web-sys` canvas style
  setters are the `set_fill_style_str(...)` variants; `Closure::once` for
  `setTimeout` must be a **zero-arg** closure (`move ||`); the event
  handlers are `onmousedown/onwheel` etc. with `client_coordinates()`.

## Testing conventions

- Host unit tests live in `#[cfg(test)]` mods in the same file (server) or
  in `crates/qwencraft-world` (geometry/terrain). Tests are
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
  count must stay under ~80% of `TERRAIN_POOL_VERTS`/`TERRAIN_POOL_IDX`
  (and the `worst_view_fits_terrain_pool_with_headroom` pins updated to the
  new worst positions).

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
6. For anything touching the protocol, `qwencraft-net`, or the remote
   backend: `./scripts/remote_test.sh` — "ALL CHECKS PASSED".
   6b. For anything touching streaming, resync, or the remote backend's
   loss path: `./scripts/wan_resync_test.sh` — "TRANSIT-LOSS TEST PASSED".
7. For anything touching `dashboard/` (or the dashboard HTTP side of
   `qwencraft-net`): `./scripts/build_dashboard.sh` + rebuild
   `qwencraft-net` + `./scripts/dashboard_test.sh` — "ALL CHECKS PASSED",
   and commit the regenerated `dashboard/dist`.
8. Update README.md if user-visible behavior changed; keep the version
   pins in lockstep if you touched them.
9. Commit on `main` with a message explaining *why* (root cause), not just
   *what*.
