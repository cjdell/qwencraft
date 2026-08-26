use std::time::Instant;
use rustcraft_server::Server;

fn main() {
    let mut s = Server::new(1337);
    // Warmup: run until the initial ring is mostly streamed.
    for _ in 0..600 {
        s.tick(1.0 / 60.0);
    }
    // Measure steady-state ticks (player standing still).
    let n = 600;
    let t = Instant::now();
    for _ in 0..n {
        s.tick(1.0 / 60.0);
    }
    let steady_us = t.elapsed().as_micros() as f64 / n as f64;
    // Measure an early tick (heavy streaming/generation phase).
    let mut s2 = Server::new(1337);
    for _ in 0..10 {
        s2.tick(1.0 / 60.0);
    } // spawn settle
    let t = Instant::now();
    let m = 300;
    for _ in 0..m {
        s2.tick(1.0 / 60.0);
    }
    let busy_us = t.elapsed().as_micros() as f64 / m as f64;
    println!("BENCH server tick: steady={steady_us:.0}us busy-start={busy_us:.0}us");
}
