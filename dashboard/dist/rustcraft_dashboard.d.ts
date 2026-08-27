/* tslint:disable */
/* eslint-disable */
/**
 * The JS entry point (called from index.html after `init()`):
 * `import init, { start } from "./rustcraft-dashboard.js"; init().then(start);`
 *
 * A cdylib with an explicit `#[wasm_bindgen]` entry is used instead of a
 * binary's `main`: the wasm-bindgen glue only re-exports `#[wasm_bindgen]`
 * functions, and a binary's `main` (which takes argv) is not one.
 */
export function start(): void;
export class JSOwner {
  private constructor();
  free(): void;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
  readonly memory: WebAssembly.Memory;
  readonly start: () => void;
  readonly __wbg_jsowner_free: (a: number, b: number) => void;
  readonly __wbindgen_malloc: (a: number, b: number) => number;
  readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
  readonly __wbindgen_exn_store: (a: number) => void;
  readonly __externref_table_alloc: () => number;
  readonly __wbindgen_export_4: WebAssembly.Table;
  readonly __wbindgen_free: (a: number, b: number, c: number) => void;
  readonly __externref_drop_slice: (a: number, b: number) => void;
  readonly __wbindgen_export_7: WebAssembly.Table;
  readonly _dyn_core_f0fd674eaa06beef___ops__function__FnMut_____Output______as_wasm_bindgen_30d48ca3aae1618e___closure__WasmClosure___describe__invoke______: (a: number, b: number) => void;
  readonly closure89_externref_shim: (a: number, b: number, c: any) => void;
  readonly closure100_externref_shim: (a: number, b: number, c: any) => void;
  readonly closure114_externref_shim: (a: number, b: number, c: any, d: any) => void;
  readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;
/**
* Instantiates the given `module`, which can either be bytes or
* a precompiled `WebAssembly.Module`.
*
* @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
*
* @returns {InitOutput}
*/
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
* If `module_or_path` is {RequestInfo} or {URL}, makes a request and
* for everything else, calls `WebAssembly.instantiate` directly.
*
* @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
*
* @returns {Promise<InitOutput>}
*/
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
