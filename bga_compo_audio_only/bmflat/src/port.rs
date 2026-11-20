//! Faithful Rust port of bmflat.c (Ayuu's bmflat, Mulan PSL v2).
//!
//! Behavior parity notes (each mirrors the C source):
//! * `bm_load` strdups the source: everything after the first NUL byte is ignored.
//! * ctype functions: `char` is signed on Windows; bytes >= 0x80 go through ucrt's
//!   out-of-range path and are never space/digit/base36.
//! * The line scanner mutates the buffer (writes NULs) exactly like the C code,
//!   including the `end`/`line_len = -1` quirks for lines made entirely of spaces.
//! * `strtof`/`strtol` are re-implemented C-compatibly (whitespace skip, sign, hex
//!   floats, inf/nan, EINVAL only when nothing parses).
//! * Track 03 values are reinterpreted base36-as-base16.
//! * Sorts use the same epsilon comparators AND the same algorithm: bmflat.c calls C
//!   `qsort`, the binary links the UCRT implementation, and qsort is not stable — which
//!   note of a tie survives sort_track's duplicate collapse (and therefore long-note
//!   conversion and wave triggers) depends on qsort's exact permutation. The port
//!   transcribes the UCRT algorithm (Windows SDK 10.0.26100, ucrt/stdlib/qsort.cpp:
//!   selection sort up to 8 elements, median-of-three quicksort above) element for
//!   element and uses it for both the note and the event sort.
//! * `struct bm_event` is a C union in the original: writing `value_f` clobbers
//!   `value`/`value_a` bits. Modeled here with a raw 32-bit `bits` field.
//!   `bm_to_seq` reuses one event struct across add_event() calls, so stale fields
//!   leak between events exactly like in C (this is observable: long-note events
//!   trigger their wave via a stale `value` at the long-note start position).

pub const BM_INDEX_MAX: usize = 1296;
pub const BM_BARS_COUNT: usize = 1000;
pub const BM_BGM_TRACKS: usize = 64;
pub const BM_MSG_LEN: usize = 128;

pub const BM_BARLINE: u8 = 0;
pub const BM_TEMPO_CHANGE: u8 = 1;
pub const BM_BGA_BASE_CHANGE: u8 = 2;
pub const BM_BGA_LAYER_CHANGE: u8 = 3;
pub const BM_BGA_POOR_CHANGE: u8 = 4;
pub const BM_STOP: u8 = 5;
pub const BM_NOTE: u8 = 6;
pub const BM_NOTE_LONG: u8 = 7;
pub const BM_NOTE_OFF: u8 = 8;

#[derive(Clone)]
pub struct Log {
    pub line: i32,
    pub message: Vec<u8>,
}

pub struct Meta {
    pub player_num: i32,
    pub genre: Option<Vec<u8>>,
    pub title: Option<Vec<u8>>,
    pub artist: Option<Vec<u8>>,
    pub subartist: Option<Vec<u8>>,
    pub init_tempo: f32,
    pub play_level: i32,
    pub judge_rank: i32,
    pub gauge_total: i32,
    pub difficulty: i32,
    pub stage_file: Option<Vec<u8>>,
    pub banner: Option<Vec<u8>>,
    pub back_bmp: Option<Vec<u8>>,
}

impl Default for Meta {
    fn default() -> Self {
        Meta {
            player_num: -1,
            genre: None,
            title: None,
            artist: None,
            subartist: None,
            init_tempo: -1.0,
            play_level: -1,
            judge_rank: -1,
            gauge_total: -1,
            difficulty: -1,
            stage_file: None,
            banner: None,
            back_bmp: None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Note {
    pub beat: f32, // in fractions of the bar
    pub bar: i16,
    pub hold: bool,
    pub value: i16,
}

#[derive(Default, Clone)]
pub struct Track {
    pub notes: Vec<Note>,
}

pub struct Chart {
    pub meta: Meta,
    pub wav: Vec<Option<Vec<u8>>>,
    pub bmp: Vec<Option<Vec<u8>>>,
    pub tempo: Vec<f32>,
    pub stop: Vec<i16>,
    pub time_sig: Vec<u8>,
    pub background: Vec<Track>,
    pub background_count: usize,
    pub object: Vec<Track>,
    pub tempo_track: Track,
    pub bga_base: Track,
    pub bga_layer: Track,
    pub bga_poor: Track,
    pub ex_tempo: Track,
    pub stop_track: Track,
}

#[derive(Clone, Copy)]
pub struct Event {
    pub pos: i32,
    pub etype: u8,
    pub track: i8,
    /// C union { struct { short value; short value_a; }; float value_f; }
    /// little-endian: value = low 16 bits, value_a = high 16 bits.
    pub bits: u32,
}

impl Event {
    #[inline]
    pub fn value(&self) -> i16 {
        self.bits as u16 as i16
    }
    #[inline]
    pub fn value_a(&self) -> i16 {
        (self.bits >> 16) as u16 as i16
    }
    #[inline]
    pub fn value_f(&self) -> f32 {
        f32::from_bits(self.bits)
    }
    #[inline]
    fn set_value(&mut self, v: i16) {
        self.bits = (self.bits & 0xFFFF_0000) | (v as u16 as u32);
    }
    #[inline]
    fn set_value_a(&mut self, v: i16) {
        self.bits = (self.bits & 0x0000_FFFF) | ((v as u16 as u32) << 16);
    }
    #[inline]
    fn set_value_f(&mut self, v: f32) {
        self.bits = v.to_bits();
    }
}

#[derive(Default)]
pub struct Seq {
    pub events: Vec<Event>,
}

// --- C ctype helpers (signed-char semantics: bytes >= 0x80 are out of range) ---

#[inline]
fn is_space_or_linebreak(b: u8) -> bool {
    b == b'\r' || b == b'\n' || b == 0
}

#[inline]
fn isspace_c(b: u8) -> bool {
    if b >= 0x80 {
        return false;
    }
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

#[inline]
fn isdigit_c(b: u8) -> bool {
    b.is_ascii_digit()
}

#[inline]
fn isbase36(b: u8) -> bool {
    b.is_ascii_digit() || b.is_ascii_uppercase()
}

#[inline]
fn base36(c1: u8, c2: u8) -> i32 {
    let d = |c: u8| -> i32 {
        if c <= b'9' {
            (c - b'0') as i32
        } else {
            (c - b'A') as i32 + 10
        }
    };
    d(c1) * 36 + d(c2)
}

// --- C strtof / strtol ---

fn starts_ci(b: &[u8], word: &[u8]) -> bool {
    b.len() >= word.len()
        && b.iter()
            .zip(word.iter())
            .all(|(x, w)| (x | 0x20) == (*w | 0x20))
}

/// C strtof: the value 0 with errno unchanged when nothing parses (the UCRT sets
/// EINVAL only for an invalid base, never for a failed conversion — probed against
/// the linked ucrtbase). 0 is always outside bmflat's [0.25, 63.75] / [1, 999] ranges,
/// so None and Some(±0.0) are indistinguishable at every call site; the port mirrors
/// the C return value anyway.
pub fn c_strtof(b: &[u8]) -> Option<f32> {
    let mut i = 0usize;
    while i < b.len() && isspace_c(b[i]) {
        i += 1;
    }
    let mut neg = false;
    let mut j = i;
    if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
        neg = b[j] == b'-';
        j += 1;
    }
    // inf / infinity
    if starts_ci(&b[j..], b"inf") {
        j += 3;
        // C also consumes "inity" (j += 5); the port never reads j past this branch.
        let _ = starts_ci(&b[j..], b"inity");
        return Some(if neg {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        });
    }
    // nan / nan(chars)
    if starts_ci(&b[j..], b"nan") {
        j += 3;
        if j < b.len() && b[j] == b'(' {
            let mut k = j + 1;
            while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                k += 1;
            }
            if k < b.len() && b[k] == b')' {
                j = k + 1;
            }
        }
        let _ = j; // C's j is dead past nan(n-char-seq); keep the assignment honest
        return Some(f32::NAN);
    }
    // hexadecimal float: 0x hexdigits [. hexdigits] [p exp]
    if j + 1 < b.len() && b[j] == b'0' && (b[j + 1] | 0x20) == b'x' {
        let mut k = j + 2;
        let mut mant: f64 = 0.0;
        let mut digits = 0i32;
        let mut frac_bits = 0i32;
        let mut seen_dot = false;
        while k < b.len() {
            let c = b[k];
            if c == b'.' && !seen_dot {
                seen_dot = true;
                k += 1;
                continue;
            }
            let d = match c {
                b'0'..=b'9' => (c - b'0') as f64,
                b'a'..=b'f' => (c - b'a') as f64 + 10.0,
                b'A'..=b'F' => (c - b'A') as f64 + 10.0,
                _ => break,
            };
            mant = mant * 16.0 + d;
            digits += 1;
            if seen_dot {
                frac_bits += 4;
            }
            k += 1;
        }
        if digits > 0 {
            let mut exp: i32 = 0;
            if k < b.len() && (b[k] | 0x20) == b'p' {
                let mut m = k + 1;
                let mut eneg = false;
                if m < b.len() && (b[m] == b'+' || b[m] == b'-') {
                    eneg = b[m] == b'-';
                    m += 1;
                }
                let mut ed = 0i32;
                let mut edigits = 0;
                while m < b.len() && b[m].is_ascii_digit() {
                    if ed < 100_000 {
                        ed = ed * 10 + (b[m] - b'0') as i32;
                    }
                    edigits += 1;
                    m += 1;
                }
                if edigits > 0 {
                    exp = if eneg { -ed } else { ed };
                    // C advances k = m here; the port does not read k past this branch.
                }
            }
            let _ = j;
            let v = mant * 2f64.powi(exp - frac_bits);
            let v = if neg { -v } else { v };
            return Some(v as f32);
        }
        // No hex digits: "0x..." parses the leading "0" as a decimal number.
        // Fall through with only the sign + "0" consumed.
        return Some(if neg { -0.0 } else { 0.0 });
    }
    // decimal
    let mut k = j;
    let mut int_digits = 0i32;
    let mut frac_digits = 0i32;
    let mut mant = String::new();
    while k < b.len() && b[k].is_ascii_digit() {
        mant.push(b[k] as char);
        int_digits += 1;
        k += 1;
    }
    if k < b.len() && b[k] == b'.' {
        mant.push('.');
        k += 1;
        while k < b.len() && b[k].is_ascii_digit() {
            mant.push(b[k] as char);
            frac_digits += 1;
            k += 1;
        }
    }
    if int_digits + frac_digits == 0 {
        // UCRT strtof: no conversion → ±0.0, errno unchanged (not EINVAL).
        return Some(if neg { -0.0 } else { 0.0 });
    }
    let mut exp: i64 = 0;
    let mut exp_seen = false;
    if k < b.len() && (b[k] | 0x20) == b'e' {
        let mut m = k + 1;
        let mut eneg = false;
        if m < b.len() && (b[m] == b'+' || b[m] == b'-') {
            eneg = b[m] == b'-';
            m += 1;
        }
        let mut ed = 0i64;
        let mut edigits = 0;
        while m < b.len() && b[m].is_ascii_digit() {
            if ed < 100_000 {
                ed = ed * 10 + (b[m] - b'0') as i64;
            }
            edigits += 1;
            m += 1;
        }
        if edigits > 0 {
            exp = if eneg { -ed } else { ed };
            exp_seen = true;
            // C advances k = m here; the port does not read k past this branch.
        }
    }
    let _ = exp_seen;
    let mut text = String::new();
    if neg {
        text.push('-');
    }
    text.push_str(&mant);
    if exp != 0 {
        text.push('e');
        text.push_str(&exp.to_string());
    }
    let v: f32 = text.parse().ok()?;
    Some(v)
}

/// C strtol base 10: 0 when nothing parses (UCRT leaves errno unchanged for a
/// valid base — probed against the linked ucrtbase — so bmflat's `errno != EINVAL`
/// check passes and the range check decides). Overflow saturates (ERANGE); the
/// port saturates at i64 while the 32-bit `long` saturates at i32, but every
/// bmflat range check tops out at 32767, so the verdicts coincide.
pub fn c_strtol(b: &[u8]) -> Option<i64> {
    let mut i = 0usize;
    while i < b.len() && isspace_c(b[i]) {
        i += 1;
    }
    let mut sign = 1i64;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        if b[i] == b'-' {
            sign = -1;
        }
        i += 1;
    }
    let mut acc: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        acc = acc.saturating_mul(10).saturating_add((b[i] - b'0') as i64);
        i += 1;
    }
    Some(sign.saturating_mul(acc))
}

// --- log helpers (messages are raw bytes, like snprintf output) ---

fn push_i32(out: &mut Vec<u8>, mut v: i32) {
    if v < 0 {
        out.push(b'-');
        v = v.wrapping_neg();
    }
    let mut tmp = [0u8; 12];
    let mut n = 0;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        out.push(tmp[n]);
    }
}

fn push_padded(out: &mut Vec<u8>, v: i32, width: usize) {
    let mut tmp = Vec::new();
    push_i32(&mut tmp, v);
    while tmp.len() < width {
        tmp.insert(0, b'0');
    }
    out.extend_from_slice(&tmp);
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

// --- bm_load ---

pub fn bm_load(_source: &[u8]) -> (Chart, Vec<Log>) {
    // strdup(_source): everything after the first NUL is gone.
    let end0 = _source
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(_source.len());
    let mut source: Vec<u8> = _source[..end0].to_vec();
    source.push(0); // buf[len] = 0
    let len = end0;

    let mut logs: Vec<Log> = Vec::new();
    let mut chart = Chart {
        meta: Meta::default(),
        wav: (0..BM_INDEX_MAX).map(|_| None).collect(),
        bmp: (0..BM_INDEX_MAX).map(|_| None).collect(),
        tempo: vec![-1.0; BM_INDEX_MAX],
        stop: vec![-1; BM_INDEX_MAX],
        time_sig: vec![0; BM_BARS_COUNT],
        background: Vec::new(),
        background_count: 0,
        object: (0..60).map(|_| Track::default()).collect(),
        tempo_track: Track::default(),
        bga_base: Track::default(),
        bga_layer: Track::default(),
        bga_poor: Track::default(),
        ex_tempo: Track::default(),
        stop_track: Track::default(),
    };

    let mut bg_index = vec![0usize; BM_BARS_COUNT];
    // Flat stand-in for `bool track_appeared[1000][60]`: the C code indexes it with
    // track values up to 462 (channel ZZ), wrapping past the row just like C's
    // row-major layout does for in-array offsets. Size absorbs every in-bounds write.
    let mut track_appeared = vec![false; BM_BARS_COUNT * 60 + 463];
    let mut lnobj: i32 = -1;

    let mut ptr = 0usize;
    let mut next = 0usize;
    let mut line: i32 = 1;

    while ptr != len {
        // --- line body ---
        while !is_space_or_linebreak(source[next]) {
            next += 1;
        }
        if source[next] == b'\r' && next + 1 < len && source[next + 1] == b'\n' {
            next += 1;
        }
        while ptr < next && isspace_c(source[ptr]) {
            ptr += 1;
        }
        let mut end = next as i64;
        while end >= ptr as i64 && isspace_c(source[end as usize]) {
            end -= 1;
        }
        end += 1;
        source[end as usize] = 0;

        if source[ptr] != b'#' {
            // for-update
            ptr = {
                next += 1;
                next
            };
            line += 1;
            continue;
        }
        let s_off = ptr + 1;
        let line_len: i64 = end - ptr as i64 - 1;

        let is_track_data = line_len >= 6
            && isdigit_c(source[s_off])
            && isdigit_c(source[s_off + 1])
            && isdigit_c(source[s_off + 2])
            && isdigit_c(source[s_off + 3])
            && isdigit_c(source[s_off + 4])
            && source[s_off + 5] == b':';

        if is_track_data {
            let bar = (source[s_off] as i32 * 100
                + source[s_off + 1] as i32 * 10
                + source[s_off + 2] as i32
                - b'0' as i32 * 111) as usize;
            let track = (source[s_off + 3] as i32 * 10 + source[s_off + 4] as i32
                - b'0' as i32 * 11) as i64;

            if (3..=69).contains(&track)
                && track != 5
                && track % 10 != 0
                && track_appeared[bar * 60 + track as usize]
            {
                let mut m = Vec::new();
                push_str(&mut m, "Track ");
                push_padded(&mut m, track as i32, 2);
                push_str(&mut m, " already defined previously, merging all notes");
                logs.push(Log { line, message: m });
            }
            track_appeared[bar * 60 + track as usize] = true;

            let value_bytes = &source[s_off + 6..end as usize];
            if track == 2 {
                // Time signature
                if let Some(x) = c_strtof(value_bytes) {
                    if (0.25..=63.75).contains(&x) {
                        let y = (x * 4.0f32 + 0.5f32) as i32;
                        if ((y as f32 - x * 4.0f32) as f64).abs() >= 1e-3 {
                            let mut m = Vec::new();
                            push_str(&mut m, "Inaccurate time signature, treating as ");
                            push_i32(&mut m, y);
                            push_str(&mut m, "/4");
                            logs.push(Log { line, message: m });
                        }
                        if chart.time_sig[bar] != 0 {
                            let mut m = Vec::new();
                            push_str(&mut m, "Time signature for bar ");
                            push_padded(&mut m, bar as i32, 3);
                            push_str(&mut m, " defined multiple times, overwriting");
                            logs.push(Log { line, message: m });
                        }
                        chart.time_sig[bar] = y as u8;
                    } else {
                        let mut m = Vec::new();
                        push_str(
                            &mut m,
                            "Invalid time signature, should be a multiple of 0.25 between 0.25 and 63.75 (inclusive)",
                        );
                        logs.push(Log { line, message: m });
                    }
                } else {
                    let mut m = Vec::new();
                    push_str(
                        &mut m,
                        "Invalid time signature, should be a multiple of 0.25 between 0.25 and 63.75 (inclusive)",
                    );
                    logs.push(Log { line, message: m });
                }
            } else if track == 3 {
                parse_track(line, value_bytes, &mut chart.tempo_track, bar, &mut logs);
            } else if track == 4 {
                parse_track(line, value_bytes, &mut chart.bga_base, bar, &mut logs);
            } else if track == 6 {
                parse_track(line, value_bytes, &mut chart.bga_poor, bar, &mut logs);
            } else if track == 7 {
                parse_track(line, value_bytes, &mut chart.bga_layer, bar, &mut logs);
            } else if track == 8 {
                parse_track(line, value_bytes, &mut chart.ex_tempo, bar, &mut logs);
            } else if track == 9 {
                parse_track(line, value_bytes, &mut chart.stop_track, bar, &mut logs);
            } else if (10..=69).contains(&track) && track % 10 != 0 {
                parse_track(
                    line,
                    value_bytes,
                    &mut chart.object[(track - 10) as usize],
                    bar,
                    &mut logs,
                );
            } else if track == 1 {
                if bg_index[bar] == BM_BGM_TRACKS {
                    let mut m = Vec::new();
                    push_str(&mut m, "Too many background tracks (more than 64) for bar ");
                    push_padded(&mut m, bar as i32, 3);
                    push_str(&mut m, ", ignoring");
                    logs.push(Log { line, message: m });
                } else {
                    let idx = bg_index[bar];
                    if chart.background.len() <= idx {
                        chart.background.resize_with(idx + 1, Track::default);
                    }
                    parse_track(
                        line,
                        value_bytes,
                        &mut chart.background[idx],
                        bar,
                        &mut logs,
                    );
                    bg_index[bar] += 1;
                    if chart.background_count < bg_index[bar] {
                        chart.background_count = bg_index[bar];
                    }
                }
            } else {
                let mut m = Vec::new();
                push_str(&mut m, "Unknown track ");
                m.push(source[s_off + 3]);
                m.push(source[s_off + 4]);
                push_str(&mut m, ", ignoring");
                logs.push(Log { line, message: m });
            }
        } else {
            // --- command ---
            // line_len can be -1 (a '#' on a line of only spaces): slice defensively
            let s = &source[s_off..end.max(s_off as i64) as usize];
            let ll = line_len.max(0) as usize;
            let mut arg = 0usize;
            while arg < ll && !isspace_c(s[arg]) {
                arg += 1;
            }
            let token_end = arg;
            // s[arg++] = '\0' (the byte is the space separator or the terminator)
            let after_token = arg + 1;
            let mut arg2 = after_token;
            while arg2 < ll && isspace_c(s[arg2]) {
                arg2 += 1;
            }
            if arg2 >= ll {
                let mut m = Vec::new();
                push_str(&mut m, "Command requires non-empty arguments, ignoring");
                logs.push(Log { line, message: m });
                // for-update
                ptr = {
                    next += 1;
                    next
                };
                line += 1;
                continue;
            }
            let token = &source[s_off..s_off + token_end];
            let value = source[s_off + arg2..end as usize].to_vec();

            if token == b"PLAYER" {
                checked_parse_int_log(
                    &mut chart.meta.player_num,
                    1,
                    3,
                    b"Multiple PLAYER commands, overwritten",
                    line,
                    &mut logs,
                    &value,
                );
            } else if token == b"GENRE" {
                if chart.meta.genre.is_some() {
                    let mut m = Vec::new();
                    push_str(&mut m, "Multiple GENRE commands, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.meta.genre = Some(value.clone());
            } else if token == b"TITLE" {
                if chart.meta.title.is_some() {
                    let mut m = Vec::new();
                    push_str(&mut m, "Multiple TITLE commands, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.meta.title = Some(value.clone());
            } else if token == b"ARTIST" {
                if chart.meta.artist.is_some() {
                    let mut m = Vec::new();
                    push_str(&mut m, "Multiple ARTIST commands, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.meta.artist = Some(value.clone());
            } else if token == b"SUBARTIST" {
                if chart.meta.subartist.is_some() {
                    let mut m = Vec::new();
                    push_str(&mut m, "Multiple SUBARTIST commands, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.meta.subartist = Some(value.clone());
            } else if token == b"BPM" {
                checked_parse_float(
                    &mut chart.meta.init_tempo,
                    1.0,
                    999.0,
                    Some(b"Multiple BPM commands, overwritten".to_vec()),
                    line,
                    &mut logs,
                    &value,
                );
            } else if token == b"PLAYLEVEL" {
                checked_parse_int_log(
                    &mut chart.meta.play_level,
                    1,
                    999,
                    b"Multiple PLAYLEVEL commands, overwritten",
                    line,
                    &mut logs,
                    &value,
                );
            } else if token == b"RANK" {
                checked_parse_int_log(
                    &mut chart.meta.judge_rank,
                    0,
                    3,
                    b"Multiple RANK commands, overwritten",
                    line,
                    &mut logs,
                    &value,
                );
            } else if token == b"TOTAL" {
                checked_parse_int_log(
                    &mut chart.meta.gauge_total,
                    1,
                    9999,
                    b"Multiple TOTAL commands, overwritten",
                    line,
                    &mut logs,
                    &value,
                );
            } else if token == b"DIFFICULTY" {
                checked_parse_int_log(
                    &mut chart.meta.difficulty,
                    1,
                    5,
                    b"Multiple DIFFICULTY commands, overwritten",
                    line,
                    &mut logs,
                    &value,
                );
            } else if token == b"STAGEFILE" {
                if chart.meta.stage_file.is_some() {
                    let mut m = Vec::new();
                    push_str(&mut m, "Multiple STAGEFILE commands, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.meta.stage_file = Some(value.clone());
            } else if token == b"BANNER" {
                if chart.meta.banner.is_some() {
                    let mut m = Vec::new();
                    push_str(&mut m, "Multiple BANNER commands, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.meta.banner = Some(value.clone());
            } else if token == b"BACKBMP" {
                if chart.meta.back_bmp.is_some() {
                    let mut m = Vec::new();
                    push_str(&mut m, "Multiple BACKBMP commands, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.meta.back_bmp = Some(value.clone());
            } else if token.len() >= 5
                && &token[..3] == b"WAV"
                && isbase36(token[3])
                && isbase36(token[4])
            {
                let index = base36(token[3], token[4]) as usize;
                let dup = chart.wav[index].is_some();
                if dup {
                    let mut m = Vec::new();
                    push_str(&mut m, "Wave ");
                    m.push(token[3]);
                    m.push(token[4]);
                    push_str(&mut m, " specified multiple times, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.wav[index] = Some(value);
            } else if token.len() >= 5
                && &token[..3] == b"BMP"
                && isbase36(token[3])
                && isbase36(token[4])
            {
                let index = base36(token[3], token[4]) as usize;
                let dup = chart.bmp[index].is_some();
                if dup {
                    let mut m = Vec::new();
                    push_str(&mut m, "Bitmap ");
                    m.push(token[3]);
                    m.push(token[4]);
                    push_str(&mut m, " specified multiple times, overwritten");
                    logs.push(Log { line, message: m });
                }
                chart.bmp[index] = Some(value);
            } else if token.len() >= 5
                && &token[..3] == b"BPM"
                && isbase36(token[3])
                && isbase36(token[4])
            {
                let index = base36(token[3], token[4]) as usize;
                let mut dup_msg: Option<Vec<u8>> = None;
                if chart.tempo[index] != -1.0 {
                    let mut m = Vec::new();
                    push_str(&mut m, "Tempo ");
                    m.push(token[3]);
                    m.push(token[4]);
                    push_str(&mut m, " specified multiple times, overwritten");
                    dup_msg = Some(m);
                }
                checked_parse_float(
                    &mut chart.tempo[index],
                    1.0,
                    999.0,
                    dup_msg,
                    line,
                    &mut logs,
                    &value,
                );
            } else if token.len() >= 6
                && &token[..4] == b"STOP"
                && isbase36(token[4])
                && isbase36(token[5])
            {
                let index = base36(token[4], token[5]) as usize;
                let dup = chart.stop[index] != -1;
                if dup {
                    let mut m = Vec::new();
                    push_str(&mut m, "Stop ");
                    m.push(token[4]);
                    m.push(token[5]);
                    push_str(&mut m, " specified multiple times, overwritten");
                    logs.push(Log { line, message: m });
                }
                match c_strtol(&value) {
                    Some(x) if (0..=32767).contains(&x) => {
                        chart.stop[index] = x as i16;
                    }
                    _ => {
                        let mut m = Vec::new();
                        push_str(
                            &mut m,
                            "Invalid integral value, should be between 0 and 32767 (inclusive)",
                        );
                        logs.push(Log { line, message: m });
                    }
                }
            } else if token == b"LNOBJ" {
                if isbase36(value[0]) && value.len() > 1 && isbase36(value[1]) {
                    if lnobj != -1 {
                        let mut m = Vec::new();
                        push_str(&mut m, "Multiple LNOBJ commands, overwritten");
                        logs.push(Log { line, message: m });
                    }
                    lnobj = base36(value[0], value[1]);
                } else {
                    let mut m = Vec::new();
                    push_str(&mut m, "Invalid base-36 index ");
                    m.push(value[0]);
                    // C reads s[arg+1] even when it is the line's NUL terminator
                    m.push(if value.len() > 1 { value[1] } else { 0 });
                    push_str(&mut m, ", ignoring");
                    logs.push(Log { line, message: m });
                }
            } else {
                let mut m = Vec::new();
                push_str(&mut m, "Unrecognized command ");
                m.extend_from_slice(token);
                push_str(&mut m, ", ignoring");
                logs.push(Log { line, message: m });
            }
        }

        // for-update: ptr = ++next, line++
        ptr = {
            next += 1;
            next
        };
        line += 1;
    }

    // --- Postprocessing ---

    // Reinterpret base-36 as base-16
    for note in chart.tempo_track.notes.iter_mut() {
        let x = note.value as i32;
        note.value = ((x / 36) * 16 + (x % 36)) as i16;
    }

    // Sort notes and handle coincident overwrites; also track the maximum bar number
    let mut max_bars: i32 = 0;
    for t in chart.object.iter_mut() {
        sort_track(t, &mut max_bars);
    }
    sort_track(&mut chart.tempo_track, &mut max_bars);
    sort_track(&mut chart.bga_base, &mut max_bars);
    sort_track(&mut chart.bga_layer, &mut max_bars);
    sort_track(&mut chart.bga_poor, &mut max_bars);
    sort_track(&mut chart.ex_tempo, &mut max_bars);
    sort_track(&mut chart.stop_track, &mut max_bars);

    // Handle long notes (#LNTYPE 1 semantics)
    for i in 0..20 {
        let track = &mut chart.object[i];
        let mut j = 1usize;
        while j < track.notes.len() {
            if track.notes[j].value == lnobj as i16 && track.notes[j - 1].value != -1 {
                track.notes[j].value = -1;
                track.notes[j - 1].hold = true;
                j += 1;
            }
            j += 1;
        }
    }
    for i in 40..60 {
        let track = &mut chart.object[i];
        let mut j = 1usize;
        while j < track.notes.len() {
            if track.notes[j].value == track.notes[j - 1].value {
                track.notes[j].value = -1;
                track.notes[j - 1].hold = true;
                j += 1;
            }
            j += 1;
        }
    }

    // Fill in missing time signatures
    for i in 0..=(max_bars as usize).min(BM_BARS_COUNT - 1) {
        if chart.time_sig[i] == 0 {
            chart.time_sig[i] = 4;
        }
    }

    // Defaults (with logs, line = -1)
    if chart.meta.player_num == -1 {
        chart.meta.player_num = 1;
        let mut m = Vec::new();
        push_str(&mut m, "Command PLAYER did not appear, defaulting to 1");
        logs.push(Log {
            line: -1,
            message: m,
        });
    }
    if chart.meta.genre.is_none() {
        chart.meta.genre = Some(b"(unknown)".to_vec());
        let mut m = Vec::new();
        push_str(
            &mut m,
            "Command GENRE did not appear, defaulting to strdup(\"(unknown)\")",
        );
        logs.push(Log {
            line: -1,
            message: m,
        });
    }
    if chart.meta.title.is_none() {
        chart.meta.title = Some(b"(unknown)".to_vec());
        let mut m = Vec::new();
        push_str(
            &mut m,
            "Command TITLE did not appear, defaulting to strdup(\"(unknown)\")",
        );
        logs.push(Log {
            line: -1,
            message: m,
        });
    }
    if chart.meta.artist.is_none() {
        chart.meta.artist = Some(b"(unknown)".to_vec());
        let mut m = Vec::new();
        push_str(
            &mut m,
            "Command ARTIST did not appear, defaulting to strdup(\"(unknown)\")",
        );
        logs.push(Log {
            line: -1,
            message: m,
        });
    }
    if chart.meta.subartist.is_none() {
        chart.meta.subartist = Some(b"(unknown)".to_vec());
    }
    if chart.meta.init_tempo == -1.0 {
        chart.meta.init_tempo = 130.0;
        let mut m = Vec::new();
        push_str(&mut m, "Command BPM did not appear, defaulting to 130");
        logs.push(Log {
            line: -1,
            message: m,
        });
    }
    if chart.meta.play_level == -1 {
        chart.meta.play_level = 3;
        let mut m = Vec::new();
        push_str(&mut m, "Command LEVEL did not appear, defaulting to 3");
        logs.push(Log {
            line: -1,
            message: m,
        });
    }
    if chart.meta.judge_rank == -1 {
        chart.meta.judge_rank = 3;
    }
    if chart.meta.gauge_total == -1 {
        chart.meta.gauge_total = 160;
    }
    if chart.meta.stage_file.is_none() {
        chart.meta.stage_file = Some(b"(none)".to_vec());
    }
    if chart.meta.banner.is_none() {
        chart.meta.banner = Some(b"(none)".to_vec());
    }
    if chart.meta.back_bmp.is_none() {
        chart.meta.back_bmp = Some(b"(none)".to_vec());
    }

    (chart, logs)
}

fn checked_parse_int_log(
    var: &mut i32,
    min: i64,
    max: i64,
    dup_msg: &[u8],
    line: i32,
    logs: &mut Vec<Log>,
    bytes: &[u8],
) {
    match c_strtol(bytes) {
        Some(x) if x >= min && x <= max => {
            if *var != -1 {
                let mut m = Vec::new();
                m.extend_from_slice(dup_msg);
                logs.push(Log { line, message: m });
            }
            *var = x as i32;
        }
        _ => {
            let mut m = Vec::new();
            push_str(&mut m, "Invalid integral value, should be between ");
            push_i32(&mut m, min as i32);
            push_str(&mut m, " and ");
            push_i32(&mut m, max as i32);
            push_str(&mut m, " (inclusive)");
            logs.push(Log { line, message: m });
        }
    }
}

fn checked_parse_float(
    var: &mut f32,
    min: f32,
    max: f32,
    dup_msg: Option<Vec<u8>>,
    line: i32,
    logs: &mut Vec<Log>,
    bytes: &[u8],
) {
    match c_strtof(bytes) {
        Some(x) if x >= min && x <= max => {
            if *var != -1.0 {
                let m = dup_msg.unwrap_or_default();
                logs.push(Log { line, message: m });
            }
            *var = x;
        }
        _ => {
            let mut m = Vec::new();
            // %g of the constants 1.0 and 999.0 in the C source prints "1" and "999"
            push_str(
                &mut m,
                "Invalid integral value, should be between 1 and 999 (inclusive)",
            );
            logs.push(Log { line, message: m });
        }
    }
}

fn parse_track(line: i32, s: &[u8], track: &mut Track, bar: usize, logs: &mut Vec<Log>) {
    let mut count = 0i32;
    for &b in s {
        if !isspace_c(b) {
            count += 1;
        }
    }
    count /= 2;

    let n = s.len();
    let mut p = 0usize;
    let mut i = 0i32;
    // C: for (int p = 0, q, i = 0; s[p] != '\0'; p = q + 1)
    loop {
        // p is at or past a valid position; loop condition s[p] != '\0'
        if p >= n {
            break;
        }
        while p < n && isspace_c(s[p]) {
            p += 1;
        }
        if p >= n {
            break;
        }
        let mut q = p + 1;
        while q < n && isspace_c(s[q]) {
            q += 1;
        }
        if q >= n {
            let mut m = Vec::new();
            push_str(&mut m, "Extraneous trailing character ");
            m.push(s[p]);
            push_str(&mut m, ", ignoring");
            logs.push(Log { line, message: m });
            break;
        }
        if !isbase36(s[p]) || !isbase36(s[q]) {
            let mut m = Vec::new();
            push_str(&mut m, "Invalid base-36 index ");
            m.push(s[p]);
            m.push(s[q]);
            push_str(&mut m, " at column ");
            push_i32(&mut m, p as i32 + 8);
            push_str(&mut m, ", ignoring");
            logs.push(Log { line, message: m });
            p = q + 1;
            continue;
        }
        let value = base36(s[p], s[q]);
        if value != 0 {
            let beat = (i as f32) / (count as f32);
            track.notes.push(Note {
                bar: bar as i16,
                beat,
                hold: false,
                value: value as i16,
            });
        }
        i += 1;
        p = q + 1;
    }
}

fn sort_track(track: &mut Track, max_bars: &mut i32) {
    // C: qsort(track->notes, note_count, sizeof(struct bm_note), note_time_compare)
    ucrt_qsort(&mut track.notes, &mut |lhs: &Note, rhs: &Note| {
        let diff = ((lhs.bar as i32 - rhs.bar as i32) as f32) + (lhs.beat - rhs.beat);
        let diff = diff as f64;
        if diff < -1e-6 {
            -1
        } else if diff > 1e-6 {
            1
        } else {
            0
        }
    });
    // Remove duplicates (keep the last note of each tie group)
    let mut q: i64 = -1;
    let mut last_time: f32 = -1.0;
    for p in 0..track.notes.len() {
        let cur_time = (track.notes[p].bar as f32) + track.notes[p].beat;
        if ((cur_time - last_time) as f64) > 1e-6 {
            q += 1;
        }
        if p as i64 != q {
            track.notes[q as usize] = track.notes[p];
        }
        last_time = cur_time;
    }
    track.notes.truncate((q + 1).max(0) as usize);

    if !track.notes.is_empty() {
        let last_bar = track.notes[track.notes.len() - 1].bar as i32;
        if *max_bars < last_bar {
            *max_bars = last_bar;
        }
    }
}

// --- bm_to_seq ---

pub fn bm_to_seq(chart: &Chart) -> Seq {
    let mut seq = Seq::default();
    let mut bar_start = vec![0i32; BM_BARS_COUNT];

    let mut event = Event {
        pos: 0,
        etype: 0,
        track: 0,
        bits: 0,
    };

    // Bar lines
    let mut beats: i32 = 0;
    let mut stop_at = BM_BARS_COUNT;
    for (i, start) in bar_start.iter_mut().enumerate() {
        *start = beats;
        event.pos = beats * 48;
        event.etype = BM_BARLINE;
        event.track = 0;
        event.set_value(i as i16);
        event.set_value_a(chart.time_sig[i] as i16);
        beats += chart.time_sig[i] as i32;
        seq.events.push(event);
        if chart.time_sig[i] == 0 {
            stop_at = i;
            break;
        }
    }
    let _ = stop_at;

    let pos_of = |note: &Note| -> i32 {
        bar_start[note.bar as usize] * 48
            + ((note.beat * chart.time_sig[note.bar as usize] as f32 * 48.0) as i32)
    };

    // Tempo changes (track 03)
    for note in chart.tempo_track.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_TEMPO_CHANGE;
        event.track = 3;
        event.set_value_f(note.value as f32);
        seq.events.push(event);
    }
    // Extended tempo changes (track 08)
    for note in chart.ex_tempo.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_TEMPO_CHANGE;
        event.track = 8;
        event.set_value_f(chart.tempo[note.value as usize]);
        seq.events.push(event);
    }

    // BGA changes
    for note in chart.bga_base.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_BGA_BASE_CHANGE;
        event.track = 4;
        event.set_value(note.value);
        seq.events.push(event);
    }
    for note in chart.bga_layer.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_BGA_LAYER_CHANGE;
        event.track = 7;
        event.set_value(note.value);
        seq.events.push(event);
    }
    for note in chart.bga_poor.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_BGA_POOR_CHANGE;
        event.track = 6;
        event.set_value(note.value);
        seq.events.push(event);
    }

    // Stops
    for note in chart.stop_track.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_STOP;
        event.track = 9;
        event.set_value(chart.stop[note.value as usize]);
        seq.events.push(event);
    }

    // Backgrounds
    for (i, track) in chart.background.iter().enumerate() {
        for note in track.notes.iter() {
            event.pos = pos_of(note);
            event.etype = BM_NOTE;
            event.track = -(i as i8);
            event.set_value(note.value);
            seq.events.push(event);
        }
    }

    // Objects
    for i in 0..60usize {
        for note in chart.object[i].notes.iter() {
            if note.value == -1 {
                // Release of a long note
                event.etype = BM_NOTE_LONG;
                let p = pos_of(note);
                event.set_value_a(p.wrapping_sub(event.pos) as i16);
                seq.events.push(event);
                // Add a pair of events to simplify time-range queries
                event.pos = p;
                event.etype = BM_NOTE_OFF;
                seq.events.push(event);
            } else {
                event.pos = pos_of(note);
                let mut tr = i as i32 + 10;
                if tr >= 50 {
                    tr -= 40;
                }
                event.track = tr as i8;
                event.set_value(note.value);
                if !note.hold {
                    event.etype = BM_NOTE;
                    seq.events.push(event);
                }
            }
        }
    }

    // C: qsort(seq->events, event_count, sizeof(struct bm_event), event_pos_type_compare)
    ucrt_qsort(&mut seq.events, &mut |lhs: &Event, rhs: &Event| {
        let diff = lhs.pos - rhs.pos;
        if diff == 0 {
            lhs.etype as i32 - rhs.etype as i32
        } else {
            diff
        }
    });

    seq
}

// --- UCRT qsort (faithful transcription) ---
//
// The C original is not stable, and bmflat.c's duplicate collapse keeps the LAST
// element of each tie group, so which note of a tie survives depends on qsort's
// exact permutation. This is a line-for-line transcription of the algorithm the
// binary links (Windows SDK 10.0.26100, ucrt/stdlib/qsort.cpp): element swaps,
// shortsort (selection sort) for runs of <= 8 elements, median-of-three quicksort
// with the same partition loops and the same smaller-side-first recursion/stack
// discipline. Element width does not affect the permutation: swaps move whole
// elements and every comparison order is preserved.

const UCRT_CUTOFF: usize = 8;
const UCRT_STKSIZ: usize = 8 * std::mem::size_of::<usize>() - 2;

/// UCRT `shortsort`: selects the maximum of the remaining run (strictly greater,
/// so the earliest maximum wins a tie) and swaps it into the tail position.
fn ucrt_shortsort<T>(v: &mut [T], lo: usize, mut hi: usize, comp: &mut impl FnMut(&T, &T) -> i32) {
    while hi > lo {
        let mut max = lo;
        let mut p = lo + 1;
        while p <= hi {
            if comp(&v[p], &v[max]) > 0 {
                max = p;
            }
            p += 1;
        }
        v.swap(max, hi);
        hi -= 1;
    }
}

/// UCRT `qsort` over a slice; `comp` follows the C contract (-1 / 0 / +1).
fn ucrt_qsort<T>(v: &mut [T], comp: &mut impl FnMut(&T, &T) -> i32) {
    let n = v.len();
    if n < 2 {
        return;
    }
    let mut lostk = [0usize; UCRT_STKSIZ];
    let mut histk = [0usize; UCRT_STKSIZ];
    let mut stkptr: isize = 0;
    let mut lo = 0usize;
    let mut hi = n - 1;
    loop {
        let size = hi - lo + 1;
        if size <= UCRT_CUTOFF {
            ucrt_shortsort(v, lo, hi, comp);
        } else {
            // Median of the first, middle and last elements.
            let mut mid = lo + size / 2;
            if comp(&v[lo], &v[mid]) > 0 {
                v.swap(lo, mid);
            }
            if comp(&v[lo], &v[hi]) > 0 {
                v.swap(lo, hi);
            }
            if comp(&v[mid], &v[hi]) > 0 {
                v.swap(mid, hi);
            }

            let mut loguy = lo;
            let mut higuy = hi;
            loop {
                if mid > loguy {
                    loop {
                        loguy += 1;
                        if !(loguy < mid && comp(&v[loguy], &v[mid]) <= 0) {
                            break;
                        }
                    }
                }
                if mid <= loguy {
                    loop {
                        loguy += 1;
                        if !(loguy <= hi && comp(&v[loguy], &v[mid]) <= 0) {
                            break;
                        }
                    }
                }
                loop {
                    higuy -= 1;
                    if !(higuy > mid && comp(&v[higuy], &v[mid]) > 0) {
                        break;
                    }
                }
                if higuy < loguy {
                    break;
                }
                v.swap(loguy, higuy);
                if mid == higuy {
                    mid = loguy;
                }
            }

            // Find adjacent elements equal to the partition element.
            higuy += 1;
            if mid < higuy {
                loop {
                    higuy -= 1;
                    if !(higuy > mid && comp(&v[higuy], &v[mid]) == 0) {
                        break;
                    }
                }
            }
            if mid >= higuy {
                loop {
                    higuy -= 1;
                    if !(higuy > lo && comp(&v[higuy], &v[mid]) == 0) {
                        break;
                    }
                }
            }

            // Sort the smaller sub-array first, push the bigger one. The C
            // differences are signed ptrdiff_t: loguy may end one past hi, making
            // `hi - loguy` == -1 (real charts hit this); usize arithmetic would
            // underflow where the C comparison is perfectly normal.
            if (higuy as isize - lo as isize) >= (hi as isize - loguy as isize) {
                if lo < higuy {
                    lostk[stkptr as usize] = lo;
                    histk[stkptr as usize] = higuy;
                    stkptr += 1;
                }
                if loguy < hi {
                    lo = loguy;
                    continue;
                }
            } else {
                if loguy < hi {
                    lostk[stkptr as usize] = loguy;
                    histk[stkptr as usize] = hi;
                    stkptr += 1;
                }
                if lo < higuy {
                    hi = higuy;
                    continue;
                }
            }
        }

        stkptr -= 1;
        if stkptr < 0 {
            return;
        }
        lo = lostk[stkptr as usize];
        hi = histk[stkptr as usize];
    }
}
