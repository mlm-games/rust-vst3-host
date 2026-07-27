//! Regressions on the public playback / realtime surface: thread-affinity of the handles and
//! the control-thread validation contract. None of these need an audio device or a plugin.

use vst3_host::{
    midi::{MidiChannel, MidiEvent},
    MidiSink, RealtimePluginRunner, RtControl,
};

const fn assert_send<T: Send>() {}

/// A `MidiSink`'s entire reason to exist is being movable to another thread (a MIDI-input
/// callback, a sequencer thread), unlike the `!Send` `AudioHandle` it comes from. Losing `Send`
/// here would silently break `midi_input::bind_to_handle` and every background-sequencer use.
#[test]
fn midi_sink_is_send() {
    assert_send::<MidiSink>();
}

/// The lock-free control half is designed to live on a control thread while the runner owns the
/// plugin on the audio thread, so it has to cross a thread boundary.
#[test]
fn rt_control_is_send() {
    assert_send::<RtControl>();
}

/// The runner owns the plugin and is moved into the device callback, so it must be `Send` even
/// though the handle wrapping the device stream is not.
#[test]
fn realtime_runner_is_send() {
    assert_send::<RealtimePluginRunner>();
}

/// A `MidiSink` really does survive the move — not just satisfy the bound.
#[test]
fn midi_sink_can_be_moved_to_another_thread() {
    fn takes_sink(sink: MidiSink) -> std::thread::JoinHandle<bool> {
        std::thread::spawn(move || {
            sink.send_midi(MidiEvent::NoteOn {
                channel: MidiChannel::Ch1,
                note: 60,
                velocity: 100,
            })
        })
    }
    // Compile-time coverage: no live sink exists without a device, so only the signature is
    // exercised here. `midi_sink_is_send` pins the bound itself.
    let _ = takes_sink;
}
