use std::time::Instant;
use rustcraft_server::Server;

fn main() {
    let mut s = Server::new(1337);
    for _ in 0..1200 {
        s.tick(1.0 / 60.0);
    }
    // Settled view (everything in radius generated/sent).
    let m = 300;
    let t = Instant::now();
    for _ in 0..m {
        s.tick(1.0 / 60.0);
    }
    let settled_us = t.elapsed().as_micros() as f64 / m as f64;

    // Fresh server: full generation + streaming phase.
    let mut fresh = Server::new(1337);
    for _ in 0..10 {
        fresh.tick(1.0 / 60.0);
    }
    let t = Instant::now();
    for _ in 0..m {
        fresh.tick(1.0 / 60.0);
    }
    let fresh_us = t.elapsed().as_micros() as f64 / m as f64;
    println!("BENCH tick: settled={settled_us:.0}us fresh={fresh_us:.0}us");
}
