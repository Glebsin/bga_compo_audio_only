//! Criterion benchmarks for the parse pipeline (`bm_load` / `bm_to_seq`).
//!
//! The chart text is generated in code (deterministic LCG), shaped like the
//! profiling workload in `bench/bench.bms`: dense base-36 pairs over 9 tracks,
//! a tempo change every 20 bars. Self-contained — no fixture files needed.

use std::hint::black_box;

use bmflat::port::{bm_load, bm_to_seq};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

const D36: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const TRACKS: [usize; 9] = [11, 12, 13, 14, 15, 16, 18, 19, 1];

fn lcg(seed: u64) -> impl FnMut() -> usize {
    let mut state = seed | 1;
    move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    }
}

/// Dense chart, `bars` bars: per bar 9 track lines of 16 base-36 pairs
/// (density 0.32, values 1..=16), a track-03 tempo line every 20 bars
/// (base36-as-base16 encoding, as bmflat reinterprets it).
fn gen_chart(bars: usize) -> Vec<u8> {
    let mut rand = lcg(0x2026_1120);
    let mut s = String::from("#BPM 170\r\n");
    for i in 1..=16usize {
        s.push_str(&format!("#WAV0{} s{i:02}\r\n", D36[i] as char));
    }
    for b in 0..bars {
        if b % 20 == 19 {
            let bpm = [170usize, 150, 185, 160, 175][(b / 20) % 5];
            s.push_str(&format!(
                "#{b:03}03:{}{}\r\n",
                D36[bpm / 16] as char,
                D36[bpm % 16] as char
            ));
        }
        for &t in &TRACKS {
            s.push_str(&format!("#{b:03}{t}:"));
            for _ in 0..16 {
                if rand() % 100 < 32 {
                    let v = 1 + rand() % 16;
                    s.push_str(&format!("0{}", D36[v] as char));
                } else {
                    s.push_str("00");
                }
            }
            s.push_str("\r\n");
        }
    }
    s.into_bytes()
}

fn bench_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("bm_load");
    for bars in [120usize, 900] {
        let chart = gen_chart(bars);
        group.bench_with_input(BenchmarkId::from_parameter(bars), &chart, |b, chart| {
            b.iter(|| bm_load(black_box(chart)))
        });
    }
    group.finish();
}

fn bench_to_seq(c: &mut Criterion) {
    let mut group = c.benchmark_group("bm_to_seq");
    for bars in [120usize, 900] {
        let chart = bm_load(&gen_chart(bars));
        group.bench_with_input(BenchmarkId::from_parameter(bars), &chart, |b, chart| {
            b.iter(|| bm_to_seq(black_box(chart)))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_load, bench_to_seq);
criterion_main!(benches);
