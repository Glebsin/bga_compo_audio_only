# bga_compo_audio_only

Renders the audio of a BMS chart into a WAV file. Drag a .bms / .bme / .bml
file onto the exe and you get the song back as plain audio.

## Usage

Drag and drop chart file onto `bga_compo_audio_only.exe`.
Output goes next to the exe:

    Results\<chart>\<chart>_audio.wav

It works from a terminal too:

    bga_compo_audio_only chart1.bms

After a drag and drop run the console stays open, press Enter to close it.

## Building

Needs a Rust toolchain, nothing else.

    cd app
    cargo build --release
