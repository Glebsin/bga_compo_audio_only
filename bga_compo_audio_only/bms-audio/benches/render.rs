//! Criterion benchmarks for the full render pipeline via `render_stream`
//! (parse + sample decode + mix + WAV streaming) against an in-memory sink,
//! so the loop performs no disk writes.
//!
//! Two workloads:
//! * `wav_120bars` — synthetic chart + 16 generated 1 s PCM WAV samples
//!   (self-contained, fixtures go to the OS temp dir);
//! * `jola_real` — a real chart with real Ogg samples, enabled only when the
//!   `BENCH_JOLA_DIR` environment variable points at the chart's folder.

use std::fs;
use std::hint::black_box;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::OnceLock;

use bms_audio::port::render_stream;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

// Match the shipped binary's global allocator so bench numbers are comparable
// with end-to-end wall times of the app.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const D36: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const TRACKS: [usize; 9] = [11, 12, 13, 14, 15, 16, 18, 19, 1];
const SR: usize = 44100;

fn lcg(seed: u64) -> impl FnMut() -> usize {
    let mut state = seed | 1;
    move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    }
}

/// 1 s stereo s16 44100 WAV: decaying sine plus noise (the shape of the
/// `bench/s01.wav..s16.wav` profiling samples).
fn wav_bytes(freq: f64, seed: u64) -> Vec<u8> {
    let frames = SR;
    let mut data = Vec::with_capacity(frames * 4);
    let mut state = seed | 1;
    let mut rnd = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as f64 / ((u64::MAX >> 33) as f64) - 0.5
    };
    for j in 0..frames {
        let t = j as f64 / SR as f64;
        let env = (-4.0 * t).exp();
        let v = 12000.0 * env * (2.0 * std::f64::consts::PI * freq * t).sin() + 600.0 * env * rnd();
        let s = v.clamp(-32768.0, 32767.0) as i16;
        let s2 = (v * 0.92).clamp(-32768.0, 32767.0) as i16;
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&s2.to_le_bytes());
    }
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&(SR as u32).to_le_bytes());
    out.extend_from_slice(&((SR * 4) as u32).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// Writes the 16 sample WAVs once per process into the OS temp dir.
fn fixture_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("bms_audio_bench_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 1..=16usize {
            let freq = 180.0 * 2f64.powf((i - 1) as f64 / 8.0);
            fs::write(
                dir.join(format!("s{i:02}.wav")),
                wav_bytes(freq, i as u64 * 7919),
            )
            .unwrap();
        }
        dir
    })
}

/// Same generator shape as `bmflat/benches/parse.rs`, but `#WAVxx` lines name
/// the sample files (extensionless: the renderer probes .ogg/.wav/...).
fn gen_chart(bars: usize) -> Vec<u8> {
    let mut rand = lcg(0x2026_1120);
    let mut s = String::from("#BPM 170\r\n");
    for (i, ch) in D36.iter().enumerate().skip(1).take(16) {
        s.push_str(&format!("#WAV0{ch} s{i:02}\r\n"));
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

/// Renders `chart` from `dir` into a reusable in-memory buffer.
fn bench_dir_chart(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
    chart: Vec<u8>,
    dir: &PathBuf,
    out_bytes: u64,
) {
    // Sanity: the pipeline must actually decode samples, not render silence
    // (a silent render is ~16x faster and would fake a huge speedup).
    {
        let mut probe = Vec::new();
        let (status, _) = render_stream(&chart, Some(dir.as_path()), &mut probe);
        assert_eq!(status, 0, "render failed");
        let non_zero = probe.iter().filter(|&&b| b != 0).count();
        assert!(
            non_zero * 2 > probe.len(),
            "bench decode produced (near-)silence — sample lookup is broken (dir = {:?}, non-zero {}/{})",
            dir,
            non_zero,
            probe.len()
        );
    }
    let mut buf: Vec<u8> = Vec::with_capacity(out_bytes as usize + (1 << 20));
    group.throughput(Throughput::Bytes(out_bytes));
    group.bench_with_input(
        BenchmarkId::new("render_stream", name),
        &chart,
        |b, chart| {
            b.iter(|| {
                buf.clear();
                let mut cur = Cursor::new(&mut buf);
                let r = render_stream(black_box(chart), Some(dir.as_path()), &mut cur);
                black_box(r)
            })
        },
    );
}

fn bench_render_wav(c: &mut Criterion) {
    let dir = fixture_dir();
    let chart = gen_chart(120);
    let mut group = c.benchmark_group("bms_audio");
    // 120 bars at ~170 BPM ≈ 169 s of audio ≈ 29.8 MB of PCM.
    bench_dir_chart(&mut group, "wav_120bars", chart, dir, 29_800_000);
    group.finish();
}

fn bench_render_jola(c: &mut Criterion) {
    let Ok(dir) = std::env::var("BENCH_JOLA_DIR") else {
        return; // real-chart workload is opt-in: set BENCH_JOLA_DIR to run it
    };
    let dir = PathBuf::from(dir);
    let Ok(chart) = fs::read(dir.join("JoLa_HARDEST.bms")) else {
        return;
    };
    let mut group = c.benchmark_group("bms_audio");
    group.sample_size(10);
    // JoLa_HARDEST renders 5 242 019 stereo frames ≈ 20.9 MB of PCM.
    bench_dir_chart(&mut group, "jola_real", chart, &dir, 20_968_076);
    group.finish();
}

criterion_group!(benches, bench_render_wav, bench_render_jola);
criterion_main!(benches);
