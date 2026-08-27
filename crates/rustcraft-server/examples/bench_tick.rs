//! Server tick benchmark: baseline (player only) plus the NPC load test at
//! increasing counts. The NPC load exercises per-agent physics against the
//! local block windows: watch ms/tick scale with the count, and the window
//! hit rate / solid fallbacks prove physics runs on the cache, not the
//! chunk buffers.

use std::time::Instant;
use rustcraft_server::{Action, Server};

fn settle(s: &mut Server, n: u32) {
    for _ in 0..n {
        s.tick(1.0 / 60.0);
    }
}

fn measure(s: &mut Server, n: u32) -> f64 {
    let t = Instant::now();
    for _ in 0..n {
        s.tick(1.0 / 60.0);
    }
    t.elapsed().as_micros() as f64 / n as f64
}

fn main() {
    // Baseline: player standing still (steady-state ticks).
    let mut s = Server::new(1337);
    settle(&mut s, 600);
    let steady_us = measure(&mut s, 600);
    // Early ticks (heavy streaming/generation phase).
    let mut s = Server::new(1337);
    settle(&mut s, 10); // spawn settle
    let busy_us = measure(&mut s, 300);

    // NPC loads: (count, spacing) pairs from the in-game dials' range.
    for (count, spacing) in [(64u32, 16.0f32), (256, 16.0), (1024, 24.0)] {
        let mut s = Server::new(1337);
        settle(&mut s, 120);
        s.set_npc_load(count, spacing);
        s.push_action(Action::NpcLoad);
        // Spawn tick + warmup (land + build windows).
        settle(&mut s, 300);
        s.reset_cache_stats();
        let us = measure(&mut s, 300);
        let st = s.stats(0);
        let c = st.cache;
        let hit_pct = 100.0 * c.hits as f64 / c.lookups.max(1) as f64;
        let rebuilds_per_s = c.rebuilds as f64 / (300.0 / 60.0);
        println!(
            "BENCH npc load: {} agents @ {:.0}m: tick={us:.0}us window={hit_pct:.2}% solid-fb={} rebuilds={rebuilds_per_s:.0}/s probes={}ms/s chunks_gen={}",
            count,
            spacing,
            c.solid_misses,
            c.rebuild_probes as f64 * 1e-3 / (300.0 / 60.0),
            st.chunks_generated
        );
    }

    println!(
        "BENCH server tick: steady(player-only)={steady_us:.0}us busy-start={busy_us:.0}us"
    );
}
