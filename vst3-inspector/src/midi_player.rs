//! Minimal Standard MIDI File (.mid) playback for the inspector.
//!
//! Parses an SMF with `midly`, flattens all tracks into a single time-ordered list of
//! `(seconds, MidiEvent)`, and replays them at UI cadence onto the live plugin. Timing is
//! tempo-aware (honors `Set Tempo` meta events across the whole file) but not sample-accurate
//! — it's driven from the control thread, which is the realistic level for a host UI.

use std::time::{Duration, Instant};
use vst3_host::midi::{MidiChannel, MidiEvent};

/// Seconds elapsed over `delta_ticks` at `tempo_us_per_quarter` microseconds per quarter note,
/// given the file's `ticks_per_quarter` division.
pub fn ticks_to_seconds(
    delta_ticks: u64,
    ticks_per_quarter: u16,
    tempo_us_per_quarter: u32,
) -> f64 {
    if ticks_per_quarter == 0 {
        return 0.0;
    }
    delta_ticks as f64 * (tempo_us_per_quarter as f64 / 1_000_000.0) / ticks_per_quarter as f64
}

/// Map a midly MIDI message on `channel` (0-based) to a library [`MidiEvent`], or `None` for
/// messages the library doesn't carry (program change, pitch bend, aftertouch, sysex...).
pub fn map_message(channel: u8, msg: &midly::MidiMessage) -> Option<MidiEvent> {
    let ch = MidiChannel::from_index(channel)?;
    Some(match msg {
        // A NoteOn with velocity 0 is the running-status idiom for NoteOff.
        midly::MidiMessage::NoteOn { key, vel } if vel.as_int() == 0 => MidiEvent::NoteOff {
            channel: ch,
            note: key.as_int(),
            velocity: 0,
        },
        midly::MidiMessage::NoteOn { key, vel } => MidiEvent::NoteOn {
            channel: ch,
            note: key.as_int(),
            velocity: vel.as_int(),
        },
        midly::MidiMessage::NoteOff { key, vel } => MidiEvent::NoteOff {
            channel: ch,
            note: key.as_int(),
            velocity: vel.as_int(),
        },
        midly::MidiMessage::Controller { controller, value } => MidiEvent::ControlChange {
            channel: ch,
            controller: controller.as_int(),
            value: value.as_int(),
        },
        _ => return None,
    })
}

/// Convert an absolute tick to seconds using a sorted tempo map (`(abs_tick, us_per_quarter)`,
/// first entry at tick 0).
fn seconds_for_tick(abs_tick: u64, tpq: u16, tempo_map: &[(u64, u32)]) -> f64 {
    let mut secs = 0.0;
    let mut last_tick = 0u64;
    let mut cur_tempo = tempo_map.first().map(|&(_, us)| us).unwrap_or(500_000);
    for &(tick, us) in tempo_map {
        if tick >= abs_tick {
            break;
        }
        if tick > last_tick {
            secs += ticks_to_seconds(tick - last_tick, tpq, cur_tempo);
            last_tick = tick;
        }
        cur_tempo = us;
    }
    secs + ticks_to_seconds(abs_tick - last_tick, tpq, cur_tempo)
}

/// Flatten an SMF into a time-ordered `(seconds, MidiEvent)` list. Errors on SMPTE timecode
/// files (only metrical/PPQ timing is supported).
pub fn flatten(smf: &midly::Smf) -> Result<Vec<(f64, MidiEvent)>, String> {
    let tpq = match smf.header.timing {
        midly::Timing::Metrical(t) => t.as_int(),
        midly::Timing::Timecode(..) => {
            return Err("SMPTE timecode MIDI files are not supported (only PPQ/metrical)".into())
        }
    };
    if tpq == 0 {
        return Err("invalid MIDI file: zero ticks-per-quarter".into());
    }

    // First pass: gather all tempo changes and raw (abs_tick, event) pairs across every track.
    let mut tempo_changes: Vec<(u64, u32)> = vec![(0, 500_000)]; // default 120 BPM at tick 0
    let mut raw: Vec<(u64, MidiEvent)> = Vec::new();
    for track in &smf.tracks {
        let mut abs_tick: u64 = 0;
        for ev in track {
            abs_tick += ev.delta.as_int() as u64;
            match ev.kind {
                midly::TrackEventKind::Meta(midly::MetaMessage::Tempo(us)) => {
                    tempo_changes.push((abs_tick, us.as_int()));
                }
                midly::TrackEventKind::Midi { channel, message } => {
                    if let Some(e) = map_message(channel.as_int(), &message) {
                        raw.push((abs_tick, e));
                    }
                }
                _ => {}
            }
        }
    }
    tempo_changes.sort_by_key(|&(t, _)| t);

    // Second pass: convert each event's tick to seconds and sort by time.
    let mut events: Vec<(f64, MidiEvent)> = raw
        .into_iter()
        .map(|(tick, e)| (seconds_for_tick(tick, tpq, &tempo_changes), e))
        .collect();
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(events)
}

/// A gap between two [`MidiFilePlayer::tick`] calls longer than this means the control thread
/// stalled (a modal dialog, a plugin load) rather than merely rendering a slow frame.
const STALL_THRESHOLD: Duration = Duration::from_millis(250);

/// How far the player is allowed to advance on the tick that discovers a stall. The rest of the
/// stall is absorbed by sliding the time origin, so playback resumes where it left off instead of
/// flushing minutes of backlog into a single frame.
const CATCH_UP_AFTER_STALL: Duration = Duration::from_millis(50);

/// Hard ceiling on the events one `tick` may hand out, so even a pathologically dense file can't
/// overrun the host's control ring in one frame. Anything past it is simply due on the next tick.
const MAX_EVENTS_PER_TICK: usize = 512;

/// Plays a loaded SMF by handing out events as their scheduled time arrives.
#[derive(Default)]
pub struct MidiFilePlayer {
    events: Vec<(f64, MidiEvent)>,
    next_idx: usize,
    start: Option<Instant>,
    /// When `tick` last ran, used to notice a stalled control thread.
    last_tick: Option<Instant>,
    playing: bool,
    loaded_name: Option<String>,
}

impl MidiFilePlayer {
    /// Load and flatten a `.mid` file. Replaces any previously loaded file (stops playback).
    pub fn load(&mut self, path: &std::path::Path) -> Result<(), String> {
        let data = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
        let smf = midly::Smf::parse(&data).map_err(|e| format!("parse failed: {e}"))?;
        self.events = flatten(&smf)?;
        self.next_idx = 0;
        self.start = None;
        self.last_tick = None;
        self.playing = false;
        self.loaded_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        Ok(())
    }

    /// The loaded file's name, if any.
    pub fn loaded_name(&self) -> Option<&str> {
        self.loaded_name.as_deref()
    }

    /// Number of scheduled events in the loaded file.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Start (or restart) playback from the beginning.
    pub fn play(&mut self, now: Instant) {
        if self.events.is_empty() {
            return;
        }
        self.next_idx = 0;
        self.start = Some(now);
        self.last_tick = Some(now);
        self.playing = true;
    }

    /// Stop playback. The caller should send an all-notes-off afterward to kill ringing notes.
    pub fn stop(&mut self) {
        self.playing = false;
        self.start = None;
        self.last_tick = None;
        self.next_idx = 0;
    }

    /// Whether playback is active.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Return the events due by `now`, advancing the cursor. Sets `playing = false` when the
    /// file finishes.
    ///
    /// The caller drives this from its UI loop, which can stall arbitrarily long (a modal file
    /// dialog, a plugin load). Rather than handing the whole backlog over at once — a burst that
    /// overruns the host's control ring and loses note-offs — a stall slides the time origin
    /// forward, so playback continues from where it was interrupted.
    pub fn tick(&mut self, now: Instant) -> Vec<MidiEvent> {
        let Some(mut start) = self.start else {
            return Vec::new();
        };

        if let Some(previous) = self.last_tick {
            let gap = now.saturating_duration_since(previous);
            if gap > STALL_THRESHOLD {
                start += gap - CATCH_UP_AFTER_STALL;
                self.start = Some(start);
            }
        }
        self.last_tick = Some(now);

        let elapsed = now.saturating_duration_since(start).as_secs_f64();
        let mut due = Vec::new();
        while self.next_idx < self.events.len()
            && self.events[self.next_idx].0 <= elapsed
            && due.len() < MAX_EVENTS_PER_TICK
        {
            due.push(self.events[self.next_idx].1);
            self.next_idx += 1;
        }
        if self.next_idx >= self.events.len() {
            self.playing = false;
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_to_seconds_at_120bpm() {
        // 480 tpq, 500000 us/qn (120 BPM): a quarter note (480 ticks) == 0.5 s.
        assert!((ticks_to_seconds(480, 480, 500_000) - 0.5).abs() < 1e-9);
        assert!((ticks_to_seconds(960, 480, 500_000) - 1.0).abs() < 1e-9);
        // Half the tempo number (250000 us = 240 BPM) halves the time.
        assert!((ticks_to_seconds(480, 480, 250_000) - 0.25).abs() < 1e-9);
        assert_eq!(ticks_to_seconds(100, 0, 500_000), 0.0); // guard div-by-zero
    }

    #[test]
    fn note_on_zero_velocity_maps_to_note_off() {
        let on0 = midly::MidiMessage::NoteOn {
            key: 60.into(),
            vel: 0.into(),
        };
        assert!(matches!(
            map_message(0, &on0),
            Some(MidiEvent::NoteOff { note: 60, .. })
        ));
        let on = midly::MidiMessage::NoteOn {
            key: 60.into(),
            vel: 100.into(),
        };
        assert!(matches!(
            map_message(0, &on),
            Some(MidiEvent::NoteOn {
                note: 60,
                velocity: 100,
                ..
            })
        ));
    }

    #[test]
    fn program_change_is_skipped() {
        let pc = midly::MidiMessage::ProgramChange { program: 5.into() };
        assert!(map_message(0, &pc).is_none());
    }

    /// A player holding `count` note-ons spaced `spacing` seconds apart, already playing.
    fn player_with_events(count: usize, spacing: f64) -> (MidiFilePlayer, Instant) {
        let events = (0..count)
            .map(|i| {
                (
                    i as f64 * spacing,
                    MidiEvent::NoteOn {
                        channel: MidiChannel::Ch1,
                        note: 60,
                        velocity: 100,
                    },
                )
            })
            .collect();
        let mut player = MidiFilePlayer {
            events,
            ..Default::default()
        };
        let start = Instant::now();
        player.play(start);
        (player, start)
    }

    #[test]
    fn tick_hands_out_events_as_they_come_due() {
        let (mut player, start) = player_with_events(4, 0.1);
        assert_eq!(player.tick(start).len(), 1); // only the event at t=0
        assert_eq!(player.tick(start + Duration::from_millis(120)).len(), 1);
        assert!(player.is_playing());
        assert_eq!(player.tick(start + Duration::from_millis(310)).len(), 2);
        assert!(!player.is_playing()); // file exhausted
    }

    #[test]
    fn a_long_stall_resyncs_the_clock_instead_of_dumping_the_backlog() {
        // 1 event every 10 ms for a minute: a naive catch-up would hand over ~6000 at once.
        let (mut player, start) = player_with_events(6000, 0.01);
        player.tick(start);

        let due = player.tick(start + Duration::from_secs(60));
        assert!(
            due.len() <= MAX_EVENTS_PER_TICK,
            "burst of {} events after a stall",
            due.len()
        );
        // The clock slid forward, so only the events within the catch-up window are due —
        // far fewer than even the hard cap.
        assert!(
            due.len() < 20,
            "expected a resync, got {} events",
            due.len()
        );
        // Playback continues from where it was interrupted rather than jumping to the end.
        assert!(player.is_playing());
        assert!(player.next_idx < 20);
    }

    #[test]
    fn a_normal_frame_gap_does_not_resync() {
        let (mut player, start) = player_with_events(100, 0.01);
        player.tick(start);
        // 16 ms — an ordinary 60 fps frame; every event due in that window must be handed out.
        let due = player.tick(start + Duration::from_millis(160));
        assert_eq!(due.len(), 16);
    }

    #[test]
    fn a_dense_burst_is_capped_and_the_remainder_stays_queued() {
        // 2000 events all at t=0: more than one tick may hand out.
        let (mut player, start) = player_with_events(2000, 0.0);
        let first = player.tick(start);
        assert_eq!(first.len(), MAX_EVENTS_PER_TICK);
        assert!(player.is_playing());
        let second = player.tick(start + Duration::from_millis(16));
        assert_eq!(second.len(), MAX_EVENTS_PER_TICK);
    }

    #[test]
    fn seconds_for_tick_honors_tempo_change() {
        // 480 tpq; tempo 500000 (120 BPM) until tick 480, then 250000 (240 BPM).
        let map = vec![(0u64, 500_000u32), (480, 250_000)];
        // First quarter at 120 BPM = 0.5 s.
        assert!((seconds_for_tick(480, 480, &map) - 0.5).abs() < 1e-9);
        // Next quarter at 240 BPM = 0.25 s → total 0.75 s at tick 960.
        assert!((seconds_for_tick(960, 480, &map) - 0.75).abs() < 1e-9);
    }
}
