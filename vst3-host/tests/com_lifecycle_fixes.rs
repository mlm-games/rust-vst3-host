//! Real-plugin regressions for the VST3 lifecycle and split-block event timing.
//!
//! These need a plugin that actually renders audio, so they are `#[ignore]`d like the rest of
//! the plugin-backed suite; run them with `cargo test -p vst3-host --test com_lifecycle_fixes
//! -- --ignored`.

use vst3_host::prelude::*;

const PLUGIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../test_plugins/Dexed.vst3");

fn plugin_available() -> bool {
    let exists = std::path::Path::new(PLUGIN).exists();
    if !exists {
        println!("Test plugin not found at {PLUGIN}, skipping");
    }
    exists
}

fn load(sample_rate: f64, block_size: usize) -> Plugin {
    let mut host = Vst3Host::builder()
        .sample_rate(sample_rate)
        .block_size(block_size)
        .build()
        .expect("build host");
    host.load_plugin(PLUGIN).expect("load plugin")
}

/// Sum of squares over a whole multi-channel buffer region.
fn energy(buffers: &AudioBuffers, range: std::ops::Range<usize>) -> f64 {
    buffers
        .outputs
        .iter()
        .flat_map(|channel| channel[range.clone()].iter())
        .map(|&s| (s as f64) * (s as f64))
        .sum()
}

/// Render one held note and return the elapsed **seconds** by which half of the note's energy
/// has been produced — a sample-rate-independent measure of how fast the plugin's envelope
/// actually runs.
fn half_energy_time(sample_rate: f64) -> f64 {
    let block = 512usize;
    let mut plugin = load(sample_rate, block);
    plugin.start_processing().expect("start processing");
    plugin
        .send_midi_note(60, 110, MidiChannel::Ch1)
        .expect("note on");

    let blocks = (sample_rate * 2.0 / block as f64).round() as usize;
    let mut per_block = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        let mut buffers = AudioBuffers::new(0, 2, block, sample_rate);
        plugin.process_audio(&mut buffers).expect("process");
        per_block.push(energy(&buffers, 0..block));
    }
    plugin.stop_processing().ok();

    let total: f64 = per_block.iter().sum();
    assert!(
        total > 0.0,
        "the plugin produced silence at {sample_rate} Hz"
    );
    let mut running = 0.0;
    let block_seconds = block as f64 / sample_rate;
    for (index, block_energy) in per_block.iter().enumerate() {
        running += block_energy;
        if running >= total / 2.0 {
            return (index + 1) as f64 * block_seconds;
        }
    }
    blocks as f64 * block_seconds
}

/// A plugin sizes its DSP — and derives its envelope/LFO rates — when it goes active, from the
/// `ProcessSetup` it was last given. Activate it before `setupProcessing` and it prepares from
/// defaults, so how fast a note decays in wall-clock time drifts with the host's sample rate
/// instead of staying put.
#[test]
#[ignore = "Requires the bundled test plugin"]
fn note_envelope_runs_at_the_configured_sample_rate() {
    if !plugin_available() {
        return;
    }

    let low = half_energy_time(44100.0);
    let high = half_energy_time(96000.0);
    println!("half-energy time: 44.1 kHz = {low:.4}s, 96 kHz = {high:.4}s");

    let drift = (low - high).abs() / low.max(high);
    assert!(
        drift < 0.15,
        "the same note must decay over the same wall-clock time at any sample rate: \
         44.1 kHz took {low:.4}s but 96 kHz took {high:.4}s ({:.0}% drift)",
        drift * 100.0
    );
}

/// Render one held note for a second at 48 kHz and return the energy of its final quarter,
/// with the plugin's filter cutoff either parked closed or ramped open sample-accurately.
fn late_energy_with_cutoff_ramp(ramp: bool) -> f64 {
    let sample_rate = 48000.0;
    let block = 512usize;
    let mut plugin = load(sample_rate, block);
    plugin.start_processing().expect("start processing");

    let cutoff = plugin
        .get_parameters()
        .expect("parameters")
        .iter()
        .find(|p| p.name.to_lowercase().contains("cutoff"))
        .map(|p| p.id)
        .expect("the test plugin has a cutoff parameter");
    plugin
        .send_midi_note(60, 110, MidiChannel::Ch1)
        .expect("note on");

    let blocks = (sample_rate / block as f64).round() as usize;
    let mut late = 0.0;
    for index in 0..blocks {
        // Four sub-block automation points per block, so the ramp exercises the
        // sample-accurate path rather than one value per block.
        for step in 0..4 {
            let progress = (index as f64 + step as f64 / 4.0) / blocks as f64;
            let value = if ramp { 0.05 + 0.9 * progress } else { 0.05 };
            let offset = (step * block / 4) as i32;
            plugin
                .set_parameter_at(cutoff, value, offset)
                .expect("schedule automation");
        }
        let mut buffers = AudioBuffers::new(0, 2, block, sample_rate);
        plugin.process_audio(&mut buffers).expect("process");
        if index >= 3 * blocks / 4 {
            late += energy(&buffers, 0..block);
        }
    }
    plugin.stop_processing().ok();
    late
}

/// The lifecycle reordering changes how much energy a decaying note has left after a second
/// (the plugin now runs its envelope at the host's rate), so absolute energy is no longer a
/// stable yardstick. Automation itself is: opening the filter over the same note must still
/// produce more energy than leaving it closed.
#[test]
#[ignore = "Requires the bundled test plugin"]
fn sample_accurate_automation_still_opens_the_filter() {
    if !plugin_available() {
        return;
    }

    let closed = late_energy_with_cutoff_ramp(false);
    let opened = late_energy_with_cutoff_ramp(true);
    println!("late-quarter energy: cutoff closed {closed:.4}, cutoff ramped open {opened:.4}");

    assert!(
        opened > closed,
        "ramping the cutoff open must raise the output energy: closed={closed:.4} \
         opened={opened:.4}"
    );
}

/// A device can hand the host a block larger than the plugin's configured maximum, which the
/// host splits into chunks. An event scheduled late in that block belongs to the chunk that
/// contains its offset — routing everything into the first chunk fires it tens of milliseconds
/// early.
#[test]
#[ignore = "Requires the bundled test plugin"]
fn a_note_scheduled_late_in_an_oversized_block_does_not_sound_early() {
    if !plugin_available() {
        return;
    }

    let sample_rate = 48000.0;
    let block = 512usize;
    let oversized = block * 4; // one device block, four plugin chunks
    let onset = 1500i32; // inside the fourth chunk

    let mut plugin = load(sample_rate, block);
    plugin.start_processing().expect("start processing");

    // Let the plugin settle, so what we measure is the note and not its startup transient.
    let mut warmup = AudioBuffers::new(0, 2, oversized, sample_rate);
    plugin.process_audio(&mut warmup).expect("warmup");

    plugin
        .send_midi_event_at(
            MidiEvent::NoteOn {
                channel: MidiChannel::Ch1,
                note: 60,
                velocity: 110,
            },
            onset,
        )
        .expect("schedule note");

    let mut buffers = AudioBuffers::new(0, 2, oversized, sample_rate);
    plugin.process_audio(&mut buffers).expect("process");
    plugin.stop_processing().ok();

    let before = energy(&buffers, 0..onset as usize);
    let after = energy(&buffers, onset as usize..oversized);
    println!("oversized block: energy before onset {before:.6}, after {after:.6}");

    assert!(
        after > 0.0,
        "the scheduled note must sound inside the block it was scheduled in"
    );
    assert!(
        before < after / 100.0,
        "nothing should sound before the note's sample offset: {before:.6} before vs \
         {after:.6} after (the note fired early)"
    );
}
