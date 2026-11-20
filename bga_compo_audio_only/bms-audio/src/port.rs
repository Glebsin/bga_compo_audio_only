//! Faithful Rust port of bms_audio.c (the BMS -> WAV renderer).
//!
//! Behavior parity notes:
//! * argv goes through the same pipeline the C runtime applies on Windows:
//!   UTF-16 command line -> ANSI (CP_ACP, best-fit, surrogates become '?') ->
//!   quote-aware split -> MinGW-style wildcard expansion (matches replace the word;
//!   a no-match pattern made only of `?`/`*`/`.` is dropped, anything else is kept).
//! * All stderr text is CRLF (C runtime text-mode translation).
//! * Timing math, mixing, clamping and the WAV header are transcribed from the C
//!   source with the same f32/f64 widths and operation order.
//! * Decoding: WAV (PCM/float, byte-exact for PCM s16 stereo 44100) and Ogg Vorbis
//!   (best-effort, via `lewton`; bit-exactness with stb_vorbis is not claimed).
//!   MP3 is not ported: a .mp3 file behaves like a missing sample (silence).

use std::ffi::OsString;
use std::io::Write;

use bmflat::port::{BM_INDEX_MAX, BM_NOTE, BM_NOTE_LONG, BM_TEMPO_CHANGE, bm_load, bm_to_seq};

struct Wave {
    pcm: Vec<i16>, // interleaved stereo
    len: i32,      // frames
    ptr: i32,      // -1 when not playing
    present: bool,
}

impl Default for Wave {
    fn default() -> Self {
        Wave {
            pcm: Vec::new(),
            len: 0,
            ptr: -1,
            present: false,
        }
    }
}

// --- Windows ANSI plumbing (same conversions the C runtime does) ---

#[cfg(windows)]
mod win32 {
    #[link(name = "kernel32")]
    // SAFETY: kernel32 is loaded into every Windows process; these are the documented
    // ANSI code-page conversion APIs, and all pointers passed stay null or in-bounds.
    unsafe extern "system" {
        fn GetACP() -> u32;
        fn WideCharToMultiByte(
            code_page: u32,
            dw_flags: u32,
            lp_wide_char_str: *const u16,
            cch_wide_char: i32,
            lp_multi_byte_str: *mut u8,
            cb_multi_byte: i32,
            lp_default_char: *const u8,
            lp_used_default_char: *mut i32,
        ) -> i32;
        fn MultiByteToWideChar(
            code_page: u32,
            dw_flags: u32,
            lp_multi_byte_str: *const u8,
            cb_multi_byte: i32,
            lp_wide_char_str: *mut u16,
            cch_wide_char: i32,
        ) -> i32;
    }

    pub fn wide_to_ansi(w: &[u16]) -> Vec<u8> {
        if w.is_empty() {
            return Vec::new();
        }
        // SAFETY: GetACP takes no arguments and cannot fail.
        let cp = unsafe { GetACP() };
        // SAFETY: `w` holds `w.len()` valid UTF-16 units; the first call only queries the
        // required byte length, the second writes exactly that many bytes into `out`.
        unsafe {
            let len = WideCharToMultiByte(
                cp,
                0,
                w.as_ptr(),
                w.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            if len <= 0 {
                return Vec::new();
            }
            let mut out = vec![0u8; len as usize];
            WideCharToMultiByte(
                cp,
                0,
                w.as_ptr(),
                w.len() as i32,
                out.as_mut_ptr(),
                len,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            out
        }
    }

    pub fn ansi_to_wide(b: &[u8]) -> Vec<u16> {
        if b.is_empty() {
            return Vec::new();
        }
        // SAFETY: GetACP takes no arguments and cannot fail.
        let cp = unsafe { GetACP() };
        // SAFETY: `b` holds `b.len()` valid ANSI bytes; the first call only queries the
        // required unit length, the second writes exactly that many units into `out`.
        unsafe {
            let len =
                MultiByteToWideChar(cp, 0, b.as_ptr(), b.len() as i32, std::ptr::null_mut(), 0);
            if len <= 0 {
                return Vec::new();
            }
            let mut out = vec![0u16; len as usize];
            MultiByteToWideChar(cp, 0, b.as_ptr(), b.len() as i32, out.as_mut_ptr(), len);
            out
        }
    }
}

fn os_to_ansi(a: &OsString) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = a.encode_wide().collect();
        win32::wide_to_ansi(&w)
    }
    #[cfg(not(windows))]
    {
        a.to_string_lossy().into_owned().into_bytes()
    }
}

fn ansi_path(b: &[u8]) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        std::path::PathBuf::from(OsString::from_wide(&win32::ansi_to_wide(b)))
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

fn read_file(b: &[u8]) -> Option<Vec<u8>> {
    std::fs::read(ansi_path(b)).ok()
}

// --- MinGW CRT-style wildcard expansion ---

fn has_wildcard(b: &[u8]) -> bool {
    b.iter().any(|&c| c == b'?' || c == b'*' || c == b'[')
}

fn fnmatch(pat: &[u8], name: &[u8]) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            b'*' => (0..=n.len()).any(|k| inner(&p[1..], &n[k..])),
            b'?' => !n.is_empty() && inner(&p[1..], &n[1..]),
            b'[' => {
                if n.is_empty() {
                    return false;
                }
                let mut i = 1usize;
                let mut neg = false;
                if i < p.len() && (p[i] == b'!' || p[i] == b'^') {
                    neg = true;
                    i += 1;
                }
                let mut matched = false;
                let mut first = true;
                while i < p.len() && (first || p[i] != b']') {
                    first = false;
                    if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                        if n[0] >= p[i] && n[0] <= p[i + 2] {
                            matched = true;
                        }
                        i += 3;
                    } else {
                        if n[0] == p[i] {
                            matched = true;
                        }
                        i += 1;
                    }
                }
                if i >= p.len() {
                    // Unterminated class: '[' is a literal character
                    return n[0] == b'[' && inner(&p[1..], &n[1..]);
                }
                matched != neg && inner(&p[i + 1..], &n[1..])
            }
            c => !n.is_empty() && n[0] == c && inner(&p[1..], &n[1..]),
        }
    }
    inner(pat, name)
}

/// Expands one argv word like the MinGW CRT does. Returns the words that replace it.
fn glob_word(word: &[u8]) -> Vec<Vec<u8>> {
    if !has_wildcard(word) {
        return vec![word.to_vec()];
    }
    let mut matches: Vec<Vec<u8>> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(".") {
        for entry in dir.flatten() {
            #[cfg(windows)]
            let name = {
                use std::os::windows::ffi::OsStrExt;
                let w: Vec<u16> = entry.file_name().encode_wide().collect();
                win32::wide_to_ansi(&w)
            };
            #[cfg(not(windows))]
            let name = entry
                .file_name()
                .to_string_lossy()
                .into_owned()
                .into_bytes();
            if name.is_empty() || name.starts_with(b".") && (word.first() != Some(&b'.')) {
                continue;
            }
            if fnmatch(word, &name) {
                matches.push(name);
            }
        }
    }
    matches.sort();
    if !matches.is_empty() {
        return matches;
    }
    // No match: patterns made only of wildcards and dots are dropped; anything
    // containing a literal character survives as the literal word.
    if word.iter().all(|&c| matches!(c, b'?' | b'*' | b'.')) {
        Vec::new()
    } else {
        vec![word.to_vec()]
    }
}

fn build_argv(args: &[OsString]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for a in args {
        let ansi = os_to_ansi(a);
        out.extend(glob_word(&ansi));
    }
    out
}

// --- decoding ---

fn to_stereo_44100(pcm: &[i16], ch: usize, rate: u32) -> Vec<i16> {
    if ch == 0 {
        return Vec::new();
    }
    let mut stereo: Vec<i16> = Vec::with_capacity(pcm.len() * 2 / ch);
    match ch {
        1 => {
            for &s in pcm {
                stereo.push(s);
                stereo.push(s);
            }
        }
        2 => stereo.extend_from_slice(pcm),
        _ => {
            // best effort: first two channels
            for chunk in pcm.chunks(ch) {
                stereo.push(chunk[0]);
                stereo.push(*chunk.get(1).unwrap_or(&chunk[0]));
            }
        }
    }
    if rate == 44100 || rate == 0 || stereo.is_empty() {
        return stereo;
    }
    // best-effort linear resample
    let frames = stereo.len() / 2;
    if frames == 0 {
        return Vec::new();
    }
    let out_frames = ((frames as u64) * 44100 / rate as u64) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for k in 0..out_frames {
        let t = k as f64 * rate as f64 / 44100.0;
        let i0 = (t as usize).min(frames - 1);
        let i1 = (i0 + 1).min(frames - 1);
        let fr = (t - i0 as f64) as f32;
        for c in 0..2 {
            let a = stereo[i0 * 2 + c] as f32;
            let b = stereo[i1 * 2 + c] as f32;
            out.push((a + (b - a) * fr) as i16);
        }
    }
    out
}

fn decode_wav(b: &[u8]) -> Option<Vec<i16>> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut tag: u16 = 0;
    let mut channels: usize = 0;
    let mut rate: u32 = 0;
    let mut bits: u16 = 0;
    let mut have_fmt = false;
    let mut data: &[u8] = &[];
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let body = pos + 8;
        let real = size.min(b.len().saturating_sub(body));
        if id == b"fmt " && real >= 16 {
            tag = u16::from_le_bytes([b[body], b[body + 1]]);
            channels = u16::from_le_bytes([b[body + 2], b[body + 3]]) as usize;
            rate = u32::from_le_bytes([b[body + 4], b[body + 5], b[body + 6], b[body + 7]]);
            bits = u16::from_le_bytes([b[body + 14], b[body + 15]]);
            if tag == 0xFFFE && real >= 40 {
                // WAVE_FORMAT_EXTENSIBLE: sub-format GUID starts at byte 24 of fmt
                tag = u16::from_le_bytes([b[body + 24], b[body + 25]]);
            }
            have_fmt = true;
        } else if id == b"data" {
            data = &b[body..body + real];
        }
        pos = body + real + (real & 1);
        if pos <= body {
            break;
        }
    }
    if !have_fmt || channels == 0 {
        return None;
    }
    let mut samples: Vec<i16> = match (tag, bits) {
        (1, 16) => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect(),
        (1, 8) => data
            .iter()
            .map(|&x| ((x as i32 - 128) << 8) as i16)
            .collect(),
        (1, 24) => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| {
                let v = (c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16);
                let v = (v << 8) >> 8; // sign-extend 24 -> 32
                (v >> 8) as i16
            })
            .collect(),
        (1, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| {
                let v = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                (v >> 16) as i16
            })
            .collect(),
        (3, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| {
                let f = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                let s = (f * 32768.0).clamp(-32768.0, 32767.0);
                s as i16
            })
            .collect(),
        _ => return None,
    };
    if data.is_empty() {
        samples = Vec::new();
    }
    Some(to_stereo_44100(&samples, channels, rate))
}

fn decode_ogg(b: &[u8]) -> Option<Vec<i16>> {
    use symphonia::core::codecs::CODEC_TYPE_VORBIS;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let mss = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(b.to_vec())),
        Default::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension("ogg");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec == CODEC_TYPE_VORBIS)?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate?;
    let channels = track.codec_params.channels?.count();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &Default::default())
        .ok()?;

    let mut samples: Vec<i16> = Vec::new();
    // lewton kept a partial decode when a packet failed; match that.
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                // Zero-frame packets occur in the wild; SampleBuffer panics on them.
                if buf.frames() > 0 {
                    let mut sbuf = symphonia::core::audio::SampleBuffer::<i16>::new(
                        buf.frames() as u64,
                        *buf.spec(),
                    );
                    sbuf.copy_interleaved_ref(buf);
                    samples.extend_from_slice(sbuf.samples());
                }
            }
            Err(_) => break,
        }
    }
    if samples.is_empty() {
        return None;
    }
    Some(to_stereo_44100(&samples, channels, rate))
}

fn decode_file(path_ansi: &[u8]) -> Option<Vec<i16>> {
    let bytes = read_file(path_ansi)?;
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        decode_wav(&bytes)
    } else if bytes.starts_with(b"OggS") {
        decode_ogg(&bytes)
    } else {
        None
    }
}

// --- rendering ---

/// x86-64 `cvttsd2si` semantics — what the original binary's `(int)` casts produce on
/// this platform: truncation toward zero for in-range values, and `i32::MIN`
/// (0x80000000) for NaN, infinities and out-of-range values. C leaves out-of-range
/// `(int)` conversions undefined; the SSE conversion of the actual binary is
/// deterministic, so the port reproduces it instead of Rust's saturating cast.
fn cvt_int_i32(x: f64) -> i32 {
    if (-2147483648.0..=2147483647.0).contains(&x) {
        x as i32
    } else {
        i32::MIN
    }
}

fn write_u16(out: &mut impl Write, v: u16) -> std::io::Result<()> {
    out.write_all(&[(v & 0xff) as u8, (v >> 8) as u8])
}

fn write_u32(out: &mut impl Write, v: u32) -> std::io::Result<()> {
    out.write_all(&[
        (v & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        ((v >> 16) & 0xff) as u8,
        ((v >> 24) & 0xff) as u8,
    ])
}

fn eprint_bytes(b: &[u8]) {
    let mut err = std::io::stderr();
    let _ = err.write_all(b);
}

fn mix_write(
    ns: i32,
    waves: &mut [Wave],
    i32_buf: &mut Vec<i32>,
    byte_buf: &mut Vec<u8>,
    out: &mut impl Write,
) -> std::io::Result<()> {
    // Reusable scratch buffers: mix_write runs once per event, and reallocating
    // two Vecs per call showed up in the profile of long charts.
    i32_buf.clear();
    i32_buf.resize((ns as usize) * 2, 0);
    for w in waves.iter_mut() {
        if w.ptr < 0 {
            continue;
        }
        let mut j: i32 = 0;
        while j < ns && w.ptr + j < w.len {
            let idx = ((w.ptr + j) * 2) as usize;
            i32_buf[j as usize * 2] += w.pcm[idx] as i32;
            i32_buf[j as usize * 2 + 1] += w.pcm[idx + 1] as i32;
            j += 1;
        }
        w.ptr += ns;
        if w.ptr >= w.len {
            w.ptr = -1;
        }
    }
    byte_buf.clear();
    byte_buf.reserve((ns as usize) * 4);
    for &v in i32_buf.iter() {
        // The C code clamps with two ifs; clamp() yields the same result.
        let s = (v >> 1).clamp(-32768, 32767);
        byte_buf.push((s & 0xff) as u8);
        byte_buf.push(((s >> 8) & 0xff) as u8);
    }
    out.write_all(byte_buf)
}

pub fn run(args: &[OsString]) -> u8 {
    let argv = build_argv(args);
    if argv.is_empty() {
        eprint_bytes(b"Drag and drop a .bms/.bme/.bml file onto this executable.\r\n");
        return 1;
    }
    // The Results folder lives next to the executable, not next to the chart:
    // <exe dir>\Results\<chart stem>\<chart stem>_audio.wav. The exe dir stays a
    // Unicode PathBuf (it can contain characters outside the ANSI code page), only
    // the chart stem comes from the ANSI argv bytes, via ansi_to_wide.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut status = 0u8;
    for chart in &argv {
        let Some(src) = read_file(chart) else {
            let mut m = Vec::new();
            m.extend_from_slice(b"Cannot read ");
            m.extend_from_slice(chart);
            m.extend_from_slice(b"\r\n");
            eprint_bytes(&m);
            status = 1;
            continue;
        };
        let bms_dir: Option<Vec<u8>> = {
            let s1 = chart.iter().rposition(|&c| c == b'/');
            let s2 = chart.iter().rposition(|&c| c == b'\\');
            s1.max(s2).map(|i| chart[..=i].to_vec())
        };
        let out_path = results_wav_path(&exe_dir, chart);
        if render_to(&src, bms_dir.as_deref(), &out_path) != 0 {
            status = 1;
        }
    }
    status
}

/// Builds `<exe dir>\Results\<chart stem>\<chart stem>_audio.wav`.
fn results_wav_path(exe_dir: &std::path::Path, chart: &[u8]) -> std::path::PathBuf {
    let last_sep = chart
        .iter()
        .rposition(|&c| c == b'/' || c == b'\\')
        .map_or(0, |i| i + 1);
    let stem_end = chart[last_sep..]
        .iter()
        .rposition(|&c| c == b'.')
        .map_or(chart.len(), |dot| last_sep + dot);
    let stem = &chart[last_sep..stem_end];
    let mut wav_name = stem.to_vec();
    wav_name.extend_from_slice(b"_audio.wav");
    let mut out = exe_dir.to_path_buf();
    out.push("Results");
    out.push(os_from_ansi(stem));
    out.push(os_from_ansi(&wav_name));
    out
}

/// ANSI argv bytes → OsString without further loss (the bytes are already what the
/// C runtime would have seen); the folder part of the path stays Unicode.
#[cfg(windows)]
fn os_from_ansi(b: &[u8]) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&win32::ansi_to_wide(b))
}

#[cfg(not(windows))]
fn os_from_ansi(b: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(b).into_owned())
}

fn render_to(src: &[u8], bms_dir: Option<&[u8]>, out_path: &std::path::Path) -> u8 {
    if let Some(dir) = out_path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        let mut m = Vec::new();
        m.extend_from_slice(b"Cannot create ");
        m.extend_from_slice(out_path.to_string_lossy().as_bytes());
        m.extend_from_slice(b"\r\n");
        eprint_bytes(&m);
        return 1;
    }
    let file = match std::fs::File::create(out_path) {
        Ok(f) => f,
        Err(_) => {
            let mut m = Vec::new();
            m.extend_from_slice(b"Cannot create ");
            m.extend_from_slice(out_path.to_string_lossy().as_bytes());
            m.extend_from_slice(b"\r\n");
            eprint_bytes(&m);
            return 1;
        }
    };
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);

    let header = |out: &mut std::io::BufWriter<std::fs::File>| -> std::io::Result<()> {
        out.write_all(b"RIFF")?;
        write_u32(out, 0)?;
        out.write_all(b"WAVEfmt ")?;
        write_u32(out, 16)?;
        write_u16(out, 1)?;
        write_u16(out, 2)?;
        write_u32(out, 44100)?;
        write_u32(out, 44100 * 4)?;
        write_u16(out, 4)?;
        write_u16(out, 16)?;
        out.write_all(b"data")?;
        write_u32(out, 0)?;
        Ok(())
    };
    if header(&mut out).is_err() {
        return 1;
    }

    // Parse diagnostics ("Line N: ..." logs) are deliberately not printed: the user
    // asked for a silent renderer (drag a chart, get a WAV). The logs are still
    // collected by bm_load for anyone who wants them; printing them was the C
    // original's behavior and is preserved in the recorded case history.
    let (chart, _logs) = bm_load(src);

    let mut waves: Vec<Wave> = (0..BM_INDEX_MAX).map(|_| Wave::default()).collect();
    let mut i32_buf: Vec<i32> = Vec::new();
    let mut byte_buf: Vec<u8> = Vec::new();

    let exts: [&[u8]; 6] = [b".ogg", b".wav", b".mp3", b".OGG", b".WAV", b".MP3"];
    // Decode in parallel: every slot is independent (the first successful extension
    // wins per slot), so the decode order cannot change the output — byte-verified
    // against the sequential version. The C original decodes on one thread.
    let slots: Vec<usize> = (0..BM_INDEX_MAX)
        .filter(|&i| chart.wav[i].is_some())
        .collect();
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(slots.len().max(1));
    let per_thread = slots.len().div_ceil(threads).max(1);
    let decoded: Vec<(usize, Option<Vec<i16>>)> = std::thread::scope(|scope| {
        slots
            .chunks(per_thread)
            .map(|chunk| {
                let chart = &chart;
                let exts = &exts;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|&i| {
                            let name = chart.wav[i].clone()?;
                            let mut base = name;
                            if let Some(d) = base.iter().rposition(|&c| c == b'.') {
                                base.truncate(d);
                            }
                            for e in exts {
                                let mut p = Vec::new();
                                if let Some(d) = bms_dir {
                                    p.extend_from_slice(d);
                                }
                                p.extend_from_slice(&base);
                                p.extend_from_slice(e);
                                if let Some(pcm) = decode_file(&p) {
                                    return Some((i, Some(pcm)));
                                }
                            }
                            Some((i, None))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    for (i, pcm) in decoded {
        if let Some(pcm) = pcm {
            waves[i] = Wave {
                len: (pcm.len() / 2) as i32,
                pcm,
                ptr: -1,
                present: true,
            };
        }
    }

    let seq = bm_to_seq(&chart);

    let mut time: f64 = 0.0;
    let mut tempo: f64 = chart.meta.init_tempo as f64;
    let mut samples: i32 = 0;

    for i in 0..seq.events.len() {
        let ev = seq.events[i];
        let dt = ev.pos - if i > 0 { seq.events[i - 1].pos } else { 0 };
        time += (dt as f64) * (60.0f64 / 48.0f64 / tempo);
        // C: `int ns = (int)(time * sr - 1e-6) - samples;` — the subtraction wraps
        // in the binary when the cast saturates to i32::MIN.
        let ns = cvt_int_i32(time * 44100.0f64 - 1e-6).wrapping_sub(samples);

        if ns > 0 {
            if mix_write(ns, &mut waves, &mut i32_buf, &mut byte_buf, &mut out).is_err() {
                return 1;
            }
            samples = samples.wrapping_add(ns);
        }

        if ev.etype == BM_TEMPO_CHANGE {
            tempo = ev.value_f() as f64;
        } else if (ev.etype == BM_NOTE || ev.etype == BM_NOTE_LONG)
            && ev.value() >= 0
            && (ev.value() as usize) < BM_INDEX_MAX
            && waves[ev.value() as usize].present
        {
            waves[ev.value() as usize].ptr = 0;
        }
    }

    loop {
        let mut active = false;
        for w in waves.iter() {
            if w.ptr >= 0 {
                active = true;
                break;
            }
        }
        if !active {
            break;
        }
        time += 1.0f64 / 30.0f64;
        let ns = cvt_int_i32(time * 44100.0f64 - 1e-6).wrapping_sub(samples);
        if ns <= 0 {
            continue;
        }
        if mix_write(ns, &mut waves, &mut i32_buf, &mut byte_buf, &mut out).is_err() {
            return 1;
        }
        samples = samples.wrapping_add(ns);
    }

    let data_size = (samples as u32).wrapping_mul(4);
    let _ = out.flush();
    let mut file = match out.into_inner() {
        Ok(f) => f,
        Err(e) => match e.into_inner().into_inner() {
            Ok(f) => f,
            Err(_) => return 1,
        },
    };
    {
        use std::io::{Seek, SeekFrom};
        if file.seek(SeekFrom::Start(4)).is_ok() {
            let _ = write_u32(&mut file, 36u32.wrapping_add(data_size));
        }
        if file.seek(SeekFrom::Start(40)).is_ok() {
            let _ = write_u32(&mut file, data_size);
        }
        let _ = file.flush();
    }

    let mut m = Vec::new();
    m.extend_from_slice(b"Created: ");
    m.extend_from_slice(out_path.to_string_lossy().as_bytes());
    m.extend_from_slice(b"\r\n");
    eprint_bytes(&m);
    0
}
