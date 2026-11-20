# BGA_compo_audio_only

Renders the audio of a BMS chart into a WAV file. Drag a .bms / .bme / .bml
file onto the exe and you get the song back as plain audio - no video, no
notes, just the track. Handy for BGA compo entries and whenever you need the
sound of a chart without starting a player.

## Usage

Drag and drop one or more chart files onto `bga_compo_audio_only.exe`.
Output goes next to the exe:

    Results\<chart>\<chart>_audio.wav

It works from a terminal too and takes several files at once:

    bga_compo_audio_only chart1.bms chart2.bml

After a drag and drop run the console stays open, press Enter to close it.

## Building

Needs a Rust toolchain, nothing else.

    cd app
    cargo build --release

The exe is `bga_compo_audio_only.exe`. The repo ships a `.cargo/config.toml`
that points the build output to `rust-target/release/` next to the repo
folder instead of `app/target/`.

## Samples

- WAV: PCM 8/16/24/32 bit and 32-bit float, mono or stereo, any sample rate
- Ogg Vorbis
- MP3 is not supported, a `.mp3` sample counts as a missing file (silence)

Sample names come from `#WAVxx` lines and are looked up in the chart's folder.
Names without an extension are probed as `.ogg` first, then `.wav`, then
`.mp3`.
