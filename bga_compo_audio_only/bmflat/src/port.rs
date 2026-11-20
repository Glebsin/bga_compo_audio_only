//! BMS chart parser.
//!
//! Behavior notes:
//! * `bm_load` ignores everything after the first NUL byte in the source.
//! * ctype helpers use signed-char semantics: bytes >= 0x80 are never
//!   space/digit/base36.
//! * The line scanner mutates the buffer (writes NULs), including the
//!   `end`/`line_len = -1` quirks for lines made entirely of spaces.
//! * `strtof`/`strtol` re-implementations: a failed conversion yields ±0.0/0,
//!   which lies outside every call-site range, so the range check decides.
//! * Track 03 values are reinterpreted base36-as-base16.
//! * Meta fields the renderer never reads (everything except `init_tempo`) are
//!   not stored, and parse diagnostics are not collected; the WAV output is
//!   unaffected.
//! * Sorts are epsilon-comparator based and NOT stable: the duplicate collapse
//!   keeps the last element of each tie group, so the exact sort permutation is
//!   part of the output contract (see the qsort section below).
//! * `Event` is a union: writing `value_f` clobbers the `value` bits, and stale
//!   fields leak between events (long-note events trigger their wave via a
//!   stale `value` at the long-note start position).

pub const BM_INDEX_MAX: usize = 1296;
pub const BM_BARS_COUNT: usize = 1000;
pub const BM_BGM_TRACKS: usize = 64;

pub const BM_BARLINE: u8 = 0;
pub const BM_TEMPO_CHANGE: u8 = 1;
pub const BM_BGA_BASE_CHANGE: u8 = 2;
pub const BM_BGA_LAYER_CHANGE: u8 = 3;
pub const BM_BGA_POOR_CHANGE: u8 = 4;
pub const BM_STOP: u8 = 5;
pub const BM_NOTE: u8 = 6;
pub const BM_NOTE_LONG: u8 = 7;
pub const BM_NOTE_OFF: u8 = 8;

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
    pub init_tempo: f32,
    pub wav: Vec<Option<Vec<u8>>>,
    pub tempo: Vec<f32>,
    pub stop: Vec<i16>,
    pub time_sig: Vec<u8>,
    pub background: Vec<Track>,
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
    /// Union of { value: i16 } and { value_f: f32 } (little-endian); only the
    /// low half and `value_f` are ever read.
    pub bits: u32,
}

impl Event {
    #[inline]
    pub fn value(&self) -> i16 {
        self.bits as u16 as i16
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
    fn set_value_f(&mut self, v: f32) {
        self.bits = v.to_bits();
    }
}

#[derive(Default)]
pub struct Seq {
    pub events: Vec<Event>,
}

// --- ctype helpers (signed-char semantics: bytes >= 0x80 are out of range) ---

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

// --- strtof / strtol ---

fn starts_ci(b: &[u8], word: &[u8]) -> bool {
    b.len() >= word.len()
        && b.iter()
            .zip(word.iter())
            .all(|(x, w)| (x | 0x20) == (*w | 0x20))
}

/// Nothing parsed → ±0.0, which lies outside every call-site range, so the
/// range check decides.
fn c_strtof(b: &[u8]) -> Option<f32> {
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
        return Some(if neg {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        });
    }
    // nan / nan(chars)
    if starts_ci(&b[j..], b"nan") {
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
            let v = mant * 2f64.powi(exp - frac_bits);
            let v = if neg { -v } else { v };
            return Some(v as f32);
        }
        // No hex digits: "0x..." parses the leading "0" as a decimal number.
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
        // No conversion → ±0.0.
        return Some(if neg { -0.0 } else { 0.0 });
    }
    let mut exp: i64 = 0;
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
            // C advances k = m here; the port does not read k past this branch.
        }
    }
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

/// Nothing parsed → 0; overflow saturates. Every range check tops out at
/// 32767, so the saturation width does not change any verdict.
fn c_strtol(b: &[u8]) -> Option<i64> {
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

pub fn bm_load(_source: &[u8]) -> Chart {
    // Everything after the first NUL byte is ignored.
    let end0 = _source
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(_source.len());
    let mut source: Vec<u8> = _source[..end0].to_vec();
    source.push(0); // buf[len] = 0
    let len = end0;

    let mut chart = Chart {
        init_tempo: -1.0,
        wav: (0..BM_INDEX_MAX).map(|_| None).collect(),
        tempo: vec![-1.0; BM_INDEX_MAX],
        stop: vec![-1; BM_INDEX_MAX],
        time_sig: vec![0; BM_BARS_COUNT],
        background: Vec::new(),
        object: (0..60).map(|_| Track::default()).collect(),
        tempo_track: Track::default(),
        bga_base: Track::default(),
        bga_layer: Track::default(),
        bga_poor: Track::default(),
        ex_tempo: Track::default(),
        stop_track: Track::default(),
    };

    // Sized 1000*60 + 463: track values up to 462 (channel ZZ) index past the
    // row; the size absorbs every in-bounds write.
    let mut track_appeared = vec![false; BM_BARS_COUNT * 60 + 463];
    let mut bg_index = vec![0usize; BM_BARS_COUNT];
    let mut lnobj: i32 = -1;

    let mut ptr = 0usize;
    let mut next = 0usize;

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
            ptr = {
                next += 1;
                next
            };
            continue;
        }
        let s_off = ptr + 1;
        let line_len: i64 = end - ptr as i64 - 1;

        if line_len >= 6
            && isdigit_c(source[s_off])
            && isdigit_c(source[s_off + 1])
            && isdigit_c(source[s_off + 2])
            && isdigit_c(source[s_off + 3])
            && isdigit_c(source[s_off + 4])
            && source[s_off + 5] == b':'
        {
            handle_track_data(
                &mut chart,
                &mut track_appeared,
                &mut bg_index,
                &source[s_off..end as usize],
            );
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
            // the separator/terminator byte is consumed
            let after_token = arg + 1;
            let mut arg2 = after_token;
            while arg2 < ll && isspace_c(s[arg2]) {
                arg2 += 1;
            }
            if arg2 >= ll {
                // Command requires non-empty arguments: skip the line.
                ptr = {
                    next += 1;
                    next
                };
                continue;
            }
            let token = &source[s_off..s_off + token_end];
            let value = &source[s_off + arg2..end as usize];
            apply_command(&mut chart, &mut lnobj, token, value);
        }

        ptr = {
            next += 1;
            next
        };
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

    // Default tempo when #BPM never appeared.
    if chart.init_tempo == -1.0 {
        chart.init_tempo = 130.0;
    }

    chart
}

/// One `#NNNCC:...` track-data line: `s` spans the text between `#` and the line's
/// terminator (`NNNCC:` then the value bytes).
fn handle_track_data(
    chart: &mut Chart,
    track_appeared: &mut [bool],
    bg_index: &mut [usize],
    s: &[u8],
) {
    let bar = (s[0] as i32 * 100 + s[1] as i32 * 10 + s[2] as i32 - b'0' as i32 * 111) as usize;
    let track = (s[3] as i32 * 10 + s[4] as i32 - b'0' as i32 * 11) as i64;
    track_appeared[bar * 60 + track as usize] = true;

    if track == 2 {
        // Time signature
        if let Some(x) = c_strtof(&s[6..])
            && (0.25..=63.75).contains(&x)
        {
            let y = (x * 4.0f32 + 0.5f32) as i32;
            chart.time_sig[bar] = y as u8;
        }
    } else if track == 3 {
        parse_track(&s[6..], &mut chart.tempo_track, bar);
    } else if track == 4 {
        parse_track(&s[6..], &mut chart.bga_base, bar);
    } else if track == 6 {
        parse_track(&s[6..], &mut chart.bga_poor, bar);
    } else if track == 7 {
        parse_track(&s[6..], &mut chart.bga_layer, bar);
    } else if track == 8 {
        parse_track(&s[6..], &mut chart.ex_tempo, bar);
    } else if track == 9 {
        parse_track(&s[6..], &mut chart.stop_track, bar);
    } else if (10..=69).contains(&track) && track % 10 != 0 {
        parse_track(&s[6..], &mut chart.object[(track - 10) as usize], bar);
    } else if track == 1 {
        let idx = bg_index[bar];
        if idx == BM_BGM_TRACKS {
            // More than 64 background tracks in one bar: skip the line.
        } else {
            if chart.background.len() <= idx {
                chart.background.resize_with(idx + 1, Track::default);
            }
            parse_track(&s[6..], &mut chart.background[idx], bar);
            bg_index[bar] += 1;
        }
    }
}

/// One command line (`#TOKEN args...`).
fn apply_command(chart: &mut Chart, lnobj: &mut i32, token: &[u8], value: &[u8]) {
    // Display-only commands (PLAYER/RANK/TOTAL/STAGEFILE/...) are not stored:
    // nothing downstream reads them.
    if token == b"BPM" {
        checked_parse_float(&mut chart.init_tempo, 1.0, 999.0, value);
    } else if token.len() >= 5 && &token[..3] == b"WAV" && isbase36(token[3]) && isbase36(token[4])
    {
        let index = base36(token[3], token[4]) as usize;
        chart.wav[index] = Some(value.to_vec());
    } else if token.len() >= 5 && &token[..3] == b"BPM" && isbase36(token[3]) && isbase36(token[4])
    {
        let index = base36(token[3], token[4]) as usize;
        checked_parse_float(&mut chart.tempo[index], 1.0, 999.0, value);
    } else if token.len() >= 6 && &token[..4] == b"STOP" && isbase36(token[4]) && isbase36(token[5])
    {
        let index = base36(token[4], token[5]) as usize;
        if let Some(x) = c_strtol(value)
            && (0..=32767).contains(&x)
        {
            chart.stop[index] = x as i16;
        }
    } else if token == b"LNOBJ" && isbase36(value[0]) && value.len() > 1 && isbase36(value[1]) {
        *lnobj = base36(value[0], value[1]);
    }
}

fn checked_parse_float(var: &mut f32, min: f32, max: f32, bytes: &[u8]) {
    if let Some(x) = c_strtof(bytes)
        && (min..=max).contains(&x)
    {
        *var = x;
    }
}

fn parse_track(s: &[u8], track: &mut Track, bar: usize) {
    let count = s.iter().filter(|&&b| !isspace_c(b)).count() as i32 / 2;

    let n = s.len();
    let mut p = 0usize;
    let mut i = 0i32;
    loop {
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
            // Extraneous trailing character: stop parsing the line.
            break;
        }
        if !isbase36(s[p]) || !isbase36(s[q]) {
            // Invalid base-36 index: skip the pair.
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
        bits: 0,
    };

    // Bar lines
    let mut beats: i32 = 0;
    for (i, start) in bar_start.iter_mut().enumerate() {
        *start = beats;
        event.pos = beats * 48;
        event.etype = BM_BARLINE;
        event.set_value(i as i16);
        beats += chart.time_sig[i] as i32;
        seq.events.push(event);
        if chart.time_sig[i] == 0 {
            break;
        }
    }

    let pos_of = |note: &Note| -> i32 {
        bar_start[note.bar as usize] * 48
            + ((note.beat * chart.time_sig[note.bar as usize] as f32 * 48.0) as i32)
    };

    // Tempo changes (track 03)
    for note in chart.tempo_track.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_TEMPO_CHANGE;
        event.set_value_f(note.value as f32);
        seq.events.push(event);
    }
    // Extended tempo changes (track 08)
    for note in chart.ex_tempo.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_TEMPO_CHANGE;
        event.set_value_f(chart.tempo[note.value as usize]);
        seq.events.push(event);
    }

    // BGA changes
    for note in chart.bga_base.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_BGA_BASE_CHANGE;
        event.set_value(note.value);
        seq.events.push(event);
    }
    for note in chart.bga_layer.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_BGA_LAYER_CHANGE;
        event.set_value(note.value);
        seq.events.push(event);
    }
    for note in chart.bga_poor.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_BGA_POOR_CHANGE;
        event.set_value(note.value);
        seq.events.push(event);
    }

    // Stops
    for note in chart.stop_track.notes.iter() {
        event.pos = pos_of(note);
        event.etype = BM_STOP;
        event.set_value(chart.stop[note.value as usize]);
        seq.events.push(event);
    }

    // Backgrounds
    for track in chart.background.iter() {
        for note in track.notes.iter() {
            event.pos = pos_of(note);
            event.etype = BM_NOTE;
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
                seq.events.push(event);
                // Add a pair of events to simplify time-range queries
                event.pos = p;
                event.etype = BM_NOTE_OFF;
                seq.events.push(event);
            } else {
                event.pos = pos_of(note);
                event.set_value(note.value);
                if !note.hold {
                    event.etype = BM_NOTE;
                    seq.events.push(event);
                }
            }
        }
    }

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

// --- qsort ---
//
// The sort is not stable, and sort_track's duplicate collapse keeps the LAST
// element of each tie group, so the exact permutation is part of the output:
// selection sort for runs of <= 8 elements, median-of-three quicksort above,
// smaller side recursed first. Do not replace it with a stable sort.

const UCRT_CUTOFF: usize = 8;
const UCRT_STKSIZ: usize = 8 * std::mem::size_of::<usize>() - 2;

/// Selects the maximum of the remaining run (strictly greater, so the earliest
/// maximum wins a tie) and swaps it into the tail position.
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

/// Sorts a slice; `comp` returns -1 / 0 / +1.
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

            // Sort the smaller sub-array first, push the bigger one; loguy may
            // end one past hi, so the side sizes are compared as signed.
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
