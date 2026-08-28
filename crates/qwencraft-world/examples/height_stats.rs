use qwencraft_world::WorldGen;
fn main() {
    for seed in [1337u64, 42, 7] {
        let g = WorldGen::new(seed);
        let mut hs = Vec::new();
        for z in (-256..256).step_by(2) {
            for x in (-256..256).step_by(2) {
                hs.push(g.height(x, z));
            }
        }
        hs.sort_unstable();
        let n = hs.len();
        let pct = |p: f64| hs[(p * n as f64) as usize];
        println!("seed {seed}: min={} p5={} p25={} p50={} p75={} p95={} max={}",
            hs[0], pct(0.05), pct(0.25), pct(0.5), pct(0.75), pct(0.95), hs[n-1]);
    }
}
