//! BMS -> WAV renderer.
//!
//! Behavior notes:
//! * argv: UTF-16 command line -> ANSI (CP_ACP, best-fit) -> quote-aware split ->
//!   wildcard expansion (a no-match pattern made only of `?`/`*`/`.` is dropped,
//!   anything else is kept). Chart arguments are first read through the true
//!   UTF-16 path — drag-and-drop passes names the code page cannot represent —
//!   and fall back to the ANSI pipeline for wildcards and odd cases.
//! * All stderr text is CRLF.
//! * Timing math, mixing, clamping and the WAV header rely on exact f32/f64
//!   operation order — do not reorder.
//! * Decoding: WAV (PCM/float, byte-exact for PCM s16 stereo 44100) and Ogg
//!   Vorbis (best-effort, decoder differences are tolerated). MP3 behaves like
//!   a missing sample (silence).

use std::ffi::{OsStr, OsString};
use std::io::Write;

use bmflat::port::{
    BM_INDEX_MAX, BM_NOTE, BM_NOTE_LONG, BM_TEMPO_CHANGE, Chart, bm_load, bm_to_seq,
};

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

// --- Windows ANSI plumbing ---

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

// --- wildcard expansion ---

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

/// Expands one argv word; returns the words that replace it.
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

fn decode_ogg(b: Vec<u8>) -> Option<Vec<i16>> {
    use symphonia::core::codecs::CODEC_TYPE_VORBIS;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    // MediaSourceStream requires a 'static Box<dyn MediaSource>, so the file
    // bytes are moved in.
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(b)), Default::default());
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
    // One SampleBuffer reused across packets (the spec is constant for a Vorbis
    // stream); grown only when a later packet needs more capacity.
    let mut sbuf: Option<(symphonia::core::audio::SampleBuffer<i16>, u64)> = None;
    // On a decode error keep what was decoded so far and stop.
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                // Zero-frame packets occur in the wild; SampleBuffer panics on them.
                if buf.frames() > 0 {
                    let frames = buf.frames() as u64;
                    let cap = frames.max(8192);
                    let grow = !matches!(&sbuf, Some((_, c)) if *c >= frames);
                    if grow {
                        sbuf = Some((
                            symphonia::core::audio::SampleBuffer::<i16>::new(cap, *buf.spec()),
                            cap,
                        ));
                    }
                    let sb = &mut sbuf.as_mut().unwrap().0;
                    sb.copy_interleaved_ref(buf);
                    samples.extend_from_slice(sb.samples());
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

fn decode_file(bms_dir: Option<&std::path::Path>, rel_ansi: &[u8]) -> Option<Vec<i16>> {
    let bytes = read_rel(bms_dir, rel_ansi)?;
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        decode_wav(&bytes)
    } else if bytes.starts_with(b"OggS") {
        decode_ogg(bytes)
    } else {
        None
    }
}

/// Reads a sample file: the chart directory as a true Unicode path plus the
/// sample name bytes taken from the chart text; with no directory — the
/// relative ANSI path.
fn read_rel(bms_dir: Option<&std::path::Path>, rel_ansi: &[u8]) -> Option<Vec<u8>> {
    match bms_dir {
        None => read_file(rel_ansi),
        Some(dir) => {
            // The directory is used verbatim (the fallback path and drive roots
            // like `G:\` end with a separator); the relative part is appended,
            // with one separator added only when the directory does not end
            // with one.
            #[cfg(windows)]
            let ends_with_sep = {
                use std::os::windows::ffi::OsStrExt;
                dir.as_os_str()
                    .encode_wide()
                    .last()
                    .is_some_and(|c| c == '\\' as u16 || c == '/' as u16)
            };
            #[cfg(not(windows))]
            let ends_with_sep = dir.as_os_str().to_string_lossy().ends_with(['\\', '/']);
            let mut p: std::ffi::OsString = dir.into();
            if !ends_with_sep {
                p.push(std::ffi::OsStr::new(&std::path::MAIN_SEPARATOR.to_string()));
            }
            p.push(os_from_ansi(rel_ansi));
            std::fs::read(std::path::PathBuf::from(p)).ok()
        }
    }
}

// --- rendering ---

/// Truncating cast with x86-64 `cvttsd2si` semantics: out-of-range and NaN map
/// to `i32::MIN` instead of Rust's saturating cast.
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
    active: &mut Vec<usize>,
    i32_buf: &mut Vec<i32>,
    byte_buf: &mut Vec<u8>,
    out: &mut impl Write,
) -> std::io::Result<()> {
    // Reusable scratch buffers. `active` holds the indices of currently playing
    // waves, at most one entry each.
    let byte_len = (ns as usize) * 4;

    // Single contributor: i32_buf[j] == pcm[j] as i32, so the clamp can never
    // bind ((i16 as i32) >> 1 ∈ [-16384, 16383]) — plain halve-and-pack.
    if active.len() == 1 {
        let i = active[0];
        let w = &mut waves[i];
        let frames = (ns.min(w.len - w.ptr).max(0)) as usize;
        byte_buf.clear();
        byte_buf.resize(byte_len, 0);
        if frames > 0 {
            let pcm = &w.pcm[(w.ptr as usize) * 2..][..frames * 2];
            // A u16 view turns the pack into a 16-bit arithmetic shift that
            // vectorizes; little-endian bytes match the two stored bytes per sample.
            if let Ok(dst) = bytemuck::try_cast_slice_mut::<u8, u16>(byte_buf) {
                for (d, &s) in dst.iter_mut().zip(pcm.iter()) {
                    *d = (s as i32 >> 1) as u16;
                }
            } else {
                let (pairs, _) = byte_buf.as_chunks_mut::<2>();
                for (dst, &s) in pairs.iter_mut().zip(pcm.iter()) {
                    let s = s as i32 >> 1;
                    dst[0] = (s & 0xff) as u8;
                    dst[1] = (s >> 8) as u8;
                }
            }
        }
        w.ptr += ns;
        if w.ptr >= w.len {
            w.ptr = -1;
            active.clear();
        }
        return out.write_all(byte_buf);
    }

    i32_buf.clear();
    i32_buf.resize((ns as usize) * 2, 0);
    let mut keep = 0usize;
    for k in 0..active.len() {
        let i = active[k];
        let w = &mut waves[i];
        // Active waves always have ptr < len (the epilogue below resets to -1).
        let frames = ns.min(w.len - w.ptr).max(0) as usize;
        if frames > 0 {
            let buf = &mut i32_buf[..frames * 2];
            let pcm = &w.pcm[(w.ptr as usize) * 2..][..frames * 2];
            // Flat zip: no bounds checks; the widening i16 -> i32 add vectorizes.
            for (d, &s) in buf.iter_mut().zip(pcm.iter()) {
                *d += s as i32;
            }
        }
        w.ptr += ns;
        if w.ptr >= w.len {
            w.ptr = -1;
        } else {
            active[keep] = i;
            keep += 1;
        }
    }
    active.truncate(keep);
    byte_buf.clear();
    byte_buf.resize(byte_len, 0);
    if let Ok(dst) = bytemuck::try_cast_slice_mut::<u8, i16>(byte_buf) {
        // i16 view: shift + narrow compile to packed arithmetic, matching clamp
        // followed by the two byte stores.
        for (d, &v) in dst.iter_mut().zip(i32_buf.iter()) {
            *d = (v >> 1).clamp(-32768, 32767) as i16;
        }
    } else {
        let (pairs, _) = byte_buf.as_chunks_mut::<2>();
        for (dst, &v) in pairs.iter_mut().zip(i32_buf.iter()) {
            let s = (v >> 1).clamp(-32768, 32767);
            dst[0] = (s & 0xff) as u8;
            dst[1] = (s >> 8) as u8;
        }
    }
    out.write_all(byte_buf)
}

pub fn run(args: &[OsString]) -> u8 {
    // Glob-expanded ANSI words per original argument: the all-empty check and
    // the fallback pipeline use them; the primary read goes through the true
    // Unicode path (see below).
    let expanded: Vec<Vec<Vec<u8>>> = args.iter().map(|a| glob_word(&os_to_ansi(a))).collect();
    if expanded.iter().all(|w| w.is_empty()) {
        eprint_bytes(b"Drag and drop a .bms/.bme/.bml file onto this executable.\r\n");
        return 1;
    }
    // The Results folder lives next to the executable, not next to the chart:
    // <exe dir>\Results\<chart stem>\<chart stem>_audio.wav.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut status = 0u8;
    for (arg, words) in args.iter().zip(&expanded) {
        // Drag-and-drop passes UTF-16 names the ANSI code page cannot represent
        // (they mangle into '?'); read the true Unicode path first and keep the
        // ANSI pipeline as the fallback for wildcards and odd cases.
        if let Ok(src) = std::fs::read(arg) {
            let dir = std::path::Path::new(arg).parent().map(|p| p.to_path_buf());
            let out_path = results_wav_path_os(&exe_dir, arg);
            if render_to(&src, dir.as_deref(), &out_path) != 0 {
                status = 1;
            }
            continue;
        }
        for chart in words {
            let Some(src) = read_file(chart) else {
                let mut m = Vec::new();
                m.extend_from_slice(b"Cannot read ");
                m.extend_from_slice(chart);
                m.extend_from_slice(b"\r\n");
                eprint_bytes(&m);
                status = 1;
                continue;
            };
            let dir = {
                let s1 = chart.iter().rposition(|&c| c == b'/');
                let s2 = chart.iter().rposition(|&c| c == b'\\');
                s1.max(s2).map(|i| ansi_path(&chart[..=i]))
            };
            let out_path = results_wav_path(&exe_dir, chart);
            if render_to(&src, dir.as_deref(), &out_path) != 0 {
                status = 1;
            }
        }
    }
    status
}

/// Same layout as `results_wav_path`, built from the true Unicode argument so
/// stems the code page cannot represent keep their real name.
fn results_wav_path_os(exe_dir: &std::path::Path, chart: &OsStr) -> std::path::PathBuf {
    let stem = std::path::Path::new(chart).file_stem().unwrap_or_default();
    let mut wav_name = stem.to_os_string();
    wav_name.push("_audio.wav");
    exe_dir.join("Results").join(stem).join(wav_name)
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

/// ANSI bytes → OsString via the code page.
#[cfg(windows)]
fn os_from_ansi(b: &[u8]) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&win32::ansi_to_wide(b))
}

#[cfg(not(windows))]
fn os_from_ansi(b: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(b).into_owned())
}

/// Patches the RIFF sizes at offsets 4 and 40 after the render loop.
fn finalize_wav(mut out: std::io::BufWriter<std::fs::File>, samples: i32) -> u8 {
    use std::io::{Seek, SeekFrom};
    let data_size = (samples as u32).wrapping_mul(4);
    let _ = out.flush();
    let mut file = match out.into_inner() {
        Ok(f) => f,
        Err(e) => match e.into_inner().into_inner() {
            Ok(f) => f,
            Err(_) => return 1,
        },
    };
    if file.seek(SeekFrom::Start(4)).is_ok() {
        let _ = write_u32(&mut file, 36u32.wrapping_add(data_size));
    }
    if file.seek(SeekFrom::Start(40)).is_ok() {
        let _ = write_u32(&mut file, data_size);
    }
    let _ = file.flush();
    0
}

/// Decodes every defined wav slot in parallel. Each slot is independent (the
/// first successful extension wins per slot) and the consumer writes results by
/// slot index, so the decode order cannot change the output. Slots are handed
/// out one by one through an atomic counter: BMS sample sets mix tiny one-shots
/// with multi-megabyte BGM stems, and static per-thread chunks would leave the
/// wall time hostage to the chunk split.
fn decode_samples(
    chart: &Chart,
    bms_dir: Option<&std::path::Path>,
) -> Vec<(usize, Option<Vec<i16>>)> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let exts: [&[u8]; 6] = [b".ogg", b".wav", b".mp3", b".OGG", b".WAV", b".MP3"];
    let slots: Vec<usize> = (0..BM_INDEX_MAX)
        .filter(|&i| chart.wav[i].is_some())
        .collect();
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(slots.len().max(1));
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        (0..threads)
            .map(|_| {
                let slots = &slots;
                let next = &next;
                let exts = &exts;
                scope.spawn(move || {
                    let mut out: Vec<(usize, Option<Vec<i16>>)> = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        if k >= slots.len() {
                            break;
                        }
                        let i = slots[k];
                        let Some(full) = chart.wav[i].as_deref() else {
                            continue; // slots is pre-filtered; unreachable
                        };
                        let dot = full.iter().rposition(|&c| c == b'.');
                        let base = dot.map_or(full, |d| &full[..d]);
                        let mut rel = base.to_vec();
                        let stem_len = rel.len();
                        let mut pcm = None;
                        for e in exts {
                            rel.truncate(stem_len);
                            rel.extend_from_slice(e);
                            if let Some(x) = decode_file(bms_dir, &rel) {
                                pcm = Some(x);
                                break;
                            }
                        }
                        out.push((i, pcm));
                    }
                    out
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    })
}

/// Prints "Cannot create <path>" and yields exit code 1.
fn fail_create(out_path: &std::path::Path) -> u8 {
    let mut m = Vec::new();
    m.extend_from_slice(b"Cannot create ");
    m.extend_from_slice(out_path.to_string_lossy().as_bytes());
    m.extend_from_slice(b"\r\n");
    eprint_bytes(&m);
    1
}

fn render_to(src: &[u8], bms_dir: Option<&std::path::Path>, out_path: &std::path::Path) -> u8 {
    if let Some(dir) = out_path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return fail_create(out_path);
    }
    let file = match std::fs::File::create(out_path) {
        Ok(f) => f,
        Err(_) => return fail_create(out_path),
    };
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let (status, samples) = render_stream(src, bms_dir, &mut out);
    if status != 0 {
        return 1;
    }
    if finalize_wav(out, samples) != 0 {
        return 1;
    }

    let mut m = Vec::new();
    m.extend_from_slice(b"Created: ");
    m.extend_from_slice(out_path.to_string_lossy().as_bytes());
    m.extend_from_slice(b"\r\n");
    eprint_bytes(&m);
    0
}

/// Render core behind `render_to`: writes the WAV header, decodes the samples,
/// mixes and streams stereo s16 frames into `out`; returns `(status,
/// frame_count)` — the RIFF size patch afterwards is `finalize_wav`'s job.
/// Public (hidden from docs) for benchmarks.
#[doc(hidden)]
pub fn render_stream(
    src: &[u8],
    bms_dir: Option<&std::path::Path>,
    mut out: &mut impl std::io::Write,
) -> (u8, i32) {
    fn header(out: &mut impl std::io::Write) -> std::io::Result<()> {
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
    }
    if header(out).is_err() {
        return (1, 0);
    }

    // Parse diagnostics are neither collected nor printed — the renderer is silent.
    let chart = bm_load(src);

    let mut waves: Vec<Wave> = (0..BM_INDEX_MAX).map(|_| Wave::default()).collect();
    let mut active: Vec<usize> = Vec::new();
    let mut i32_buf: Vec<i32> = Vec::new();
    let mut byte_buf: Vec<u8> = Vec::new();

    for (i, pcm) in decode_samples(&chart, bms_dir) {
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
    let mut tempo: f64 = chart.init_tempo as f64;
    let mut samples: i32 = 0;

    for (i, ev) in seq.events.iter().enumerate() {
        let dt = ev.pos - if i > 0 { seq.events[i - 1].pos } else { 0 };
        time += (dt as f64) * (60.0f64 / 48.0f64 / tempo);
        // The subtraction wraps when the cast saturates to i32::MIN.
        let ns = cvt_int_i32(time * 44100.0f64 - 1e-6).wrapping_sub(samples);

        if ns > 0 {
            if mix_write(
                ns,
                &mut waves,
                &mut active,
                &mut i32_buf,
                &mut byte_buf,
                &mut out,
            )
            .is_err()
            {
                return (1, 0);
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
            // Retriggering an already-playing wave restarts it (ptr = 0) without
            // duplicating its entry in the active list.
            let idx = ev.value() as usize;
            if waves[idx].ptr < 0 {
                active.push(idx);
            }
            waves[idx].ptr = 0;
        }
    }

    loop {
        // The active list empties exactly when every wave's ptr is negative.
        if active.is_empty() {
            break;
        }
        time += 1.0f64 / 30.0f64;
        let ns = cvt_int_i32(time * 44100.0f64 - 1e-6).wrapping_sub(samples);
        if ns <= 0 {
            continue;
        }
        if mix_write(
            ns,
            &mut waves,
            &mut active,
            &mut i32_buf,
            &mut byte_buf,
            &mut out,
        )
        .is_err()
        {
            return (1, 0);
        }
        samples = samples.wrapping_add(ns);
    }

    (0, samples)
}
