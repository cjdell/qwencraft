use std::time::Instant;
use qwencraft_server::{Server, Streamer};

/// Bench the per-tick cost of the simulation + one viewer's chunk streaming.
fn main() {
    let mut s = Server::new(1337);
    let mut st = Streamer::new();
    for _ in 0..1200 {
        s.tick(1.0 / 60.0);
        let dirty = s.drain_dirty();
        st.apply_edits(s.world(), &dirty);
        let vp = s.player_state().pos;
        st.tick(s.world_mut(), vp);
        let _ = st.take();
    }
    // Settled view (everything in radius generated/sent).
    let m = 300;
    let t = Instant::now();
    for _ in 0..m {
        s.tick(1.0 / 60.0);
        let dirty = s.drain_dirty();
        st.apply_edits(s.world(), &dirty);
        let vp = s.player_state().pos;
        st.tick(s.world_mut(), vp);
        let _ = st.take();
    }
    let settled_us = t.elapsed().as_micros() as f64 / m as f64;

    // Fresh server: full generation + streaming phase.
    let mut fresh = Server::new(1337);
    let mut fst = Streamer::new();
    for _ in 0..10 {
        fresh.tick(1.0 / 60.0);
        let vp = fresh.player_state().pos;
        fst.tick(fresh.world_mut(), vp);
        let _ = fst.take();
    }
    let t = Instant::now();
    for _ in 0..m {
        fresh.tick(1.0 / 60.0);
        let dirty = fresh.drain_dirty();
        fst.apply_edits(fresh.world(), &dirty);
        let vp = fresh.player_state().pos;
        fst.tick(fresh.world_mut(), vp);
        let _ = fst.take();
    }
    let fresh_us = t.elapsed().as_micros() as f64 / m as f64;
    println!("BENCH tick: settled={settled_us:.0}us fresh={fresh_us:.0}us");
}
