# RustCraft

A voxel (Minecraft-style) engine written in Rust that runs in the browser.
The world is generated procedurally from a seed, streamed on demand from an
embedded server crate, and rendered with WebGPU: solid-colour blocks with
per-vertex voxel lighting, ambient occlusion and distance fog, agents drawn
as spheres, first-person keyboard + mouse controls.

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
./scripts/verify.sh    # headless-chromium smoke test + pixel checks
./scripts/walk_test.sh # headless walk stress test (terrain pool / streaming)
cargo test             # host unit tests (worldgen, physics, streaming, …)
```

## Controls

| key | action |
| --- | ------ |
| `W A S D` | move / fly horizontally |
| `Space` / `Shift` | jump / sprint — **up / down while flying** |
| `Mouse` | look (pointer-locked) |
| `Left click` / `Right click` | break / place block |
| `F` | toggle **fly mode** (no gravity, no collision) |
| `Q` / `E` | fly speed down / up (×1.5 steps, 5 → 500 blocks/s; hold to ramp) |

While flying the HUD shows the current speed (`FLY 120 b/s`). At high
speeds the world streams in around you (terrain is generated on the fly),
so expect the landscape to pop in a few chunks behind the horizon.

## Layout

| crate / dir         | what it is                                                              |
| ------------------- | ----------------------------------------------------------------------- |
| `rustcraft-world`   | Block types, seeded noise/terrain, 16³ chunks with 26³ region payloads, chunk meshing (voxel lighting + AO), shared WGSL shader + view-projection math |
| `rustcraft-server`  | The game server: infinite lazy world (chunks generated on demand), agent simulation (player + NPCs) with a 3D surface cache, fixed-tick physics, delta-based world updates. Runs in-process in the browser; standalone later |
| `rustcraft-client`  | WebGPU (wgpu 27) renderer: shared terrain-mesh buffer pool, sphere agents, fog, first-person camera |
| `rustcraft-web`     | wasm glue: input (keyboard/pointer lock), HUD, main loop, embedded server |
| `web/`              | `index.html` page hosting the wasm app                                  |
| `scripts/`          | build / serve / verify / walk-stress test                               |

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
streaming sphere of rough (cave + mountain) terrain needs ~1.0-1.5M
vertices (`rustcraft-server`'s `pool_measure` example); the pool holds
2M vertices / 3M indices. A walk test with the cap artificially shrunken
to 300K exercises 100+ compactions per minute with zero lost chunks.

## Notes / environment quirks

- The host's Vulkan driver may be broken (glibc ABI mismatch). verify.sh
  always forces the lavapipe ICD via `VK_ICD_FILENAMES`; do not rely on
  host-side wgpu/lavapipe tests here.
- Headless Chromium does not fire `requestAnimationFrame`; the app has a
  16 ms `setInterval` fallback driver (with an 8 ms guard against
  double-rendering when both are active).
- The root filesystem can be full; run with
  `env TMPDIR=/home/cjdell/tmp` so build/chromium scratch goes to `/home`.
