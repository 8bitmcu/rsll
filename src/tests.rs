use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use midir::MidiOutputConnection;

use crate::config::{parse_key, try_parse_duration, try_parse_length_fraction, DrumsConfig, KeyMappings, Length, SceneConfig};
use crate::domain::{EngineState, MessageType, TrackEvent, quantize_tick};
use crate::engine::{compute_max_tick, handle_midi_message};

fn test_keys() -> Arc<RwLock<KeyMappings>> {
    Arc::new(RwLock::new(KeyMappings {
        metronome:    parse_key("CC11"),
        clear_track:  parse_key("CC76"),
        record_track: parse_key("CC77"),
        clear_all:    parse_key("CC79"),
        track_keys:   ["CC16", "CC17", "CC18", "CC19", "CC20", "CC21", "CC22", "CC23"]
            .iter().map(|k| parse_key(k)).collect(),
        shift:        parse_key("CC17_CH1"),
        beat4:        parse_key("48"),
        beat8:        parse_key("50"),
        beat16:       parse_key("52"),
        drum_keys:    vec![],
        drum_notes:   vec![],
        drum_channel: 9,
        metronome_led: None,
        record_led:    None,
        led_on_channel: None,
        led_off_channel: None,
        led_blink_channel: None,
        quantize: Some(parse_key("CC78")),
        instrument_select: None,
        undo_track: Some(parse_key("CC75")),
        mute_track: Some(parse_key("CC73")),
        volume_knob: None,
        bpm_knob: None,
        scene_down: None,
        scene_up: None,
    }))
}

fn empty_scenes() -> Arc<RwLock<Vec<(String, SceneConfig)>>> {
    Arc::new(RwLock::new(Vec::new()))
}

fn null_ports() -> (Arc<Mutex<Option<MidiOutputConnection>>>, Arc<Mutex<Option<MidiOutputConnection>>>) {
    (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)))
}

fn setup_test_state() -> Arc<Mutex<EngineState>> {
    let mut track_channels = HashMap::new();
    track_channels.insert(1, 0);
    track_channels.insert(2, 0);
    track_channels.insert(3, 1);
    track_channels.insert(4, 2);
    track_channels.insert(5, 3);
    track_channels.insert(6, 4);
    track_channels.insert(7, 5);
    track_channels.insert(8, 6);

    let mut tracks = HashMap::new();
    for i in 1..=8 {
        tracks.insert(i, Vec::new());
    }

    let mut track_lengths = HashMap::new();
    for i in 1..=8 {
        track_lengths.insert(i, 96_usize);
    }

    let mut track_programs = HashMap::new();
    let mut track_volumes = HashMap::new();
    let mut track_velocities = HashMap::new();
    let mut track_velocity_multipliers = HashMap::new();
    for i in 1..=8 {
        track_programs.insert(i, 0u8);
        track_volumes.insert(i, 127u8);
        track_velocities.insert(i, None);
        track_velocity_multipliers.insert(i, 1.0f32);
    }

    Arc::new(Mutex::new(EngineState {
        bpm: 120,
        ppqn: 24,
        time_signature_numerator: 4,
        time_signature_denominator: 4,
        ticks_per_measure: 96,
        max_tick: 384,
        current_tick: 0,
        is_recording: false,
        pending_record: false,
        record_armed_at: None,
        record_arm_time: Duration::ZERO,
        metronome_on: true,
        quantize_on: false,
        active_track: 1,
        track_channels,
        tracks,
        track_lengths,
        track_programs,
        track_volumes,
        track_velocities,
        track_velocity_multipliers,
        drum_velocity: None,
        drum_velocity_multiplier: 1.0,
        track_history: HashMap::new(),
        muted_tracks: HashSet::new(),
        loop_start_lens: HashMap::new(),
        held_notes: HashMap::new(),
        physically_held: HashSet::new(),
        shift_held: false,
        current_scene_idx: 0,
        scene_name: "test".to_string(),
        scene_display_name: None,
        track_ids: (1..=8).collect(),
        track_names: HashMap::new(),
        drums_name: None,
    }))
}

#[test]
fn test_toggle_metronome() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // Metronome starts as true
    assert!(state.lock().unwrap().metronome_on);

    // CC 11 value 127 should toggle it to false
    let msg = [0xB0, 11, 127];
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(!state.lock().unwrap().metronome_on);

    // CC 11 value 127 should toggle it to true again
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(state.lock().unwrap().metronome_on);

    // CC 11 value 0 (release) should be ignored
    let msg_release = [0xB0, 11, 0];
    handle_midi_message(&msg_release, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(state.lock().unwrap().metronome_on);
}

#[test]
fn test_toggle_recording() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    assert!(!state.lock().unwrap().is_recording);
    assert!(!state.lock().unwrap().pending_record);

    // CC 77 arms recording (punch-in at next downbeat), not immediate
    let msg = [0xB0, 77, 127];
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(!state.lock().unwrap().is_recording);
    assert!(state.lock().unwrap().pending_record);

    // CC 77 again cancels the armed state
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(!state.lock().unwrap().is_recording);
    assert!(!state.lock().unwrap().pending_record);
}

#[test]
fn test_select_track() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    assert_eq!(state.lock().unwrap().active_track, 1);

    // CC 18 (Pad 3) should switch active track to 3
    let msg = [0xB0, 18, 127];
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert_eq!(state.lock().unwrap().active_track, 3);
}

#[test]
fn test_note_recording() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // Enable recording and advance tick
    {
        let mut s = state.lock().unwrap();
        s.is_recording = true;
        s.current_tick = 15;
        s.active_track = 3; // mapped to channel 1 (0-indexed)
    }

    // Note On, Note 60, Velocity 100 on Channel 1 (index 0)
    let note_msg = [0x90, 60, 100];
    handle_midi_message(&note_msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    // Check if recorded
    let s = state.lock().unwrap();
    let track3_events = &s.tracks[&3];
    assert_eq!(track3_events.len(), 1);
    let event = &track3_events[0];
    assert_eq!(event.tick, 15);
    assert_eq!(event.data1, 60);
    assert_eq!(event.data2, 100);
    assert_eq!(event.message_type, MessageType::NoteOn);
    assert_eq!(event.channel, 1); // track 3 target channel is 1
}

#[test]
fn test_clear_track() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // Record some dummy note
    {
        let mut s = state.lock().unwrap();
        s.tracks.get_mut(&1).unwrap().push(TrackEvent {
            tick: 0,
            message_type: MessageType::NoteOn,
            data1: 60,
            data2: 100,
            channel: 0,
        });
    }

    assert_eq!(state.lock().unwrap().tracks[&1].len(), 1);

    // CC 76 should clear the active track (which is 1)
    let msg = [0xB0, 76, 127];
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert_eq!(state.lock().unwrap().tracks[&1].len(), 0);
}

#[test]
fn test_clear_all_tracks() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // Record notes on track 1 and track 3
    {
        let mut s = state.lock().unwrap();
        s.tracks.get_mut(&1).unwrap().push(TrackEvent {
            tick: 0,
            message_type: MessageType::NoteOn,
            data1: 60,
            data2: 100,
            channel: 0,
        });
        s.tracks.get_mut(&3).unwrap().push(TrackEvent {
            tick: 5,
            message_type: MessageType::NoteOn,
            data1: 64,
            data2: 90,
            channel: 1,
        });
    }

    assert_eq!(state.lock().unwrap().tracks[&1].len(), 1);
    assert_eq!(state.lock().unwrap().tracks[&3].len(), 1);

    // CC 78 should clear all tracks
    let msg = [0xB0, 78, 127];
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    let s = state.lock().unwrap();
    for i in 1..=8 {
        assert_eq!(s.tracks[&i].len(), 0);
    }
}

#[test]
fn test_cc17_sets_held_flag() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    assert!(!state.lock().unwrap().shift_held);

    // CC 17 on channel 0 press should set the held flag
    let press = [0xB0, 17, 127];
    handle_midi_message(&press, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(state.lock().unwrap().shift_held);

    // CC 17 release on channel 0 (value 0) should clear it
    let release = [0xB0, 17, 0];
    handle_midi_message(&release, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(!state.lock().unwrap().shift_held);
}

#[test]
fn test_loop_length_set_via_cc17_note_combo() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // Default is 96 (4 beats)
    assert_eq!(state.lock().unwrap().track_lengths[&1], 96);

    // Hold CC 17 on channel 0
    handle_midi_message(&[0xB0, 17, 127], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    // Note 50 = 8 beats = 192 ticks
    handle_midi_message(&[0x90, 50, 100], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert_eq!(state.lock().unwrap().track_lengths[&1], 192);

    // Note 52 = 16 beats = 384 ticks
    handle_midi_message(&[0x90, 52, 100], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert_eq!(state.lock().unwrap().track_lengths[&1], 384);

    // Note 48 = 4 beats = 96 ticks
    handle_midi_message(&[0x90, 48, 100], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert_eq!(state.lock().unwrap().track_lengths[&1], 96);
}

#[test]
fn test_loop_length_clips_existing_events() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // Set track 1 to 8 beats (192 ticks) first, then record notes across both halves
    {
        let mut s = state.lock().unwrap();
        s.track_lengths.insert(1, 192);
        s.tracks.get_mut(&1).unwrap().push(TrackEvent {
            tick: 50,
            message_type: MessageType::NoteOn,
            data1: 60,
            data2: 100,
            channel: 0,
        });
        s.tracks.get_mut(&1).unwrap().push(TrackEvent {
            tick: 150, // beyond 4-beat boundary
            message_type: MessageType::NoteOn,
            data1: 64,
            data2: 100,
            channel: 0,
        });
    }

    assert_eq!(state.lock().unwrap().tracks[&1].len(), 2);

    // Shrink back to 4 beats (96 ticks) via CC 17 + note 48
    handle_midi_message(&[0xB0, 17, 127], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    handle_midi_message(&[0x90, 48, 100], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    let s = state.lock().unwrap();
    assert_eq!(s.track_lengths[&1], 96);
    // tick 50 survives, tick 150 is clipped
    assert_eq!(s.tracks[&1].len(), 1);
    assert_eq!(s.tracks[&1][0].tick, 50);
}

#[test]
fn test_cc17_note_not_recorded_or_played() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    {
        let mut s = state.lock().unwrap();
        s.is_recording = true;
    }

    // Hold CC 17 on channel 0 and press note 48
    handle_midi_message(&[0xB0, 17, 127], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    handle_midi_message(&[0x90, 48, 100], &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    // The combo note should not be recorded as a musical event
    assert_eq!(state.lock().unwrap().tracks[&1].len(), 0);
}

#[test]
fn test_quantize_tick_snapping() {
    // ppqn=24, ticks_per_sixteenth=6, ticks_per_measure=96
    // Grid points at multiples of 6: 0, 6, 12, 18, ...
    assert_eq!(quantize_tick(0, 6, 96), 0);
    assert_eq!(quantize_tick(2, 6, 96), 0);   // rounds down
    assert_eq!(quantize_tick(3, 6, 96), 6);   // exactly halfway, rounds up
    assert_eq!(quantize_tick(5, 6, 96), 6);   // rounds up
    assert_eq!(quantize_tick(6, 6, 96), 6);
    assert_eq!(quantize_tick(8, 6, 96), 6);   // rounds down
    assert_eq!(quantize_tick(93, 6, 96), 90); // near end of loop
    assert_eq!(quantize_tick(94, 6, 96), 96 % 96); // wraps to 0

    // Loops longer than one measure must fold into their own length, not
    // ticks_per_measure. With a 192-tick (two-measure) loop, a tick in the
    // second measure stays there instead of wrapping back onto the first
    // (which is what happened when ticks_per_measure was passed in).
    assert_eq!(quantize_tick(100, 6, 192), 102); // second measure survives
    assert_eq!(quantize_tick(189, 6, 192), 192 % 192); // wraps only at full loop length
}

#[test]
fn test_toggle_quantize() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    assert!(!state.lock().unwrap().quantize_on);

    // Press CC 78 (dedicated quantize key) — should toggle quantize on
    let cc78_press = [0xB0, 78, 127];
    handle_midi_message(&cc78_press, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(state.lock().unwrap().quantize_on);
    assert!(!state.lock().unwrap().is_recording);

    // Press CC 78 again — should toggle quantize off
    handle_midi_message(&cc78_press, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(!state.lock().unwrap().quantize_on);

    // Press CC 77 with shift held — should arm recording (pending_record), NOT quantize
    let cc17_press = [0xB0, 17, 127];
    handle_midi_message(&cc17_press, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    let cc77_press = [0xB0, 77, 127];
    handle_midi_message(&cc77_press, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(state.lock().unwrap().pending_record);
    assert!(!state.lock().unwrap().quantize_on); // shift+record no longer toggles quantize
}

#[test]
fn test_cc17_channel_disambiguation() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // CC 17 on channel 9 (drum pads) → track select, shift_held stays false
    let cc17_ch9_press = [0xB9, 17, 127]; // 0xB9 = 0xB0 | 9
    handle_midi_message(&cc17_ch9_press, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(!state.lock().unwrap().shift_held);
    assert_eq!(state.lock().unwrap().active_track, 2); // CC 17 → track 2

    // CC 17 release on channel 9 — should NOT clear shift_held (it's already false,
    // and this release must not interfere with the quantize modifier state)
    {
        state.lock().unwrap().shift_held = true; // manually set to test the guard
    }
    let cc17_ch9_release = [0xB9, 17, 0];
    handle_midi_message(&cc17_ch9_release, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(state.lock().unwrap().shift_held); // still true — channel 9 release didn't clear it

    // CC 17 release on channel 0 — should clear shift_held
    let cc17_ch0_release = [0xB0, 17, 0];
    handle_midi_message(&cc17_ch0_release, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    assert!(!state.lock().unwrap().shift_held);
}

#[test]
fn test_recording_with_quantize() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    {
        let mut s = state.lock().unwrap();
        s.is_recording = true;
        s.quantize_on = true;
        s.current_tick = 8; // between grid points 6 and 12, rounds down to 6
    }

    let note_msg = [0x90, 60, 100];
    handle_midi_message(&note_msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    let s = state.lock().unwrap();
    assert_eq!(s.tracks[&1][0].tick, 6);
}

#[test]
fn test_recording_tick_normalized_to_track_length() {
    // Regression test for: notes recorded when current_tick >= track_length would never play back
    // because playback checks `event.tick == tick % track_length` (0..track_length range),
    // but unquantized recording was storing the raw clock tick (0..384).
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    {
        let mut s = state.lock().unwrap();
        s.is_recording = true;
        s.quantize_on = false;
        // Simulate current_tick being in the second repetition of the 96-tick loop
        s.current_tick = 100; // 100 % 96 = 4
    }

    let note_msg = [0x90, 60, 100];
    handle_midi_message(&note_msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    let s = state.lock().unwrap();
    // Should be stored as 4 (100 % 96), not 100 — otherwise it would never match in playback
    assert_eq!(s.tracks[&1][0].tick, 4);
}

#[test]
fn test_pitch_bend_recording() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    {
        let mut s = state.lock().unwrap();
        s.is_recording = true;
        s.current_tick = 20;
        s.active_track = 1; // mapped to channel 0
    }

    // Pitch bend: center-up gesture (LSB=0, MSB=96 → value 12288, slightly above center)
    let bend_msg = [0xE0, 0x00, 0x60];
    handle_midi_message(&bend_msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    let s = state.lock().unwrap();
    let events = &s.tracks[&1];
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.message_type, MessageType::PitchBend);
    assert_eq!(ev.tick, 20);
    assert_eq!(ev.data1, 0x00); // LSB
    assert_eq!(ev.data2, 0x60); // MSB
    assert_eq!(ev.channel, 0);     // active track 1 → channel 0
}

#[test]
fn test_pitch_bend_not_quantized() {
    // Pitch bend should always record at the actual tick, even when quantize is on
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    {
        let mut s = state.lock().unwrap();
        s.is_recording = true;
        s.quantize_on = true;
        s.current_tick = 8; // between grid points, would snap to 6 if quantized
    }

    let bend_msg = [0xE0, 0x00, 0x40];
    handle_midi_message(&bend_msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    let s = state.lock().unwrap();
    // Should be 8, not 6 — pitch bend ignores quantize
    assert_eq!(s.tracks[&1][0].tick, 8);
}

#[test]
fn test_recording_without_quantize_unchanged() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    {
        let mut s = state.lock().unwrap();
        s.is_recording = true;
        s.quantize_on = false;
        s.current_tick = 8;
    }

    let note_msg = [0x90, 60, 100];
    handle_midi_message(&note_msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());

    let s = state.lock().unwrap();
    assert_eq!(s.tracks[&1][0].tick, 8); // unmodified
}

#[test]
fn test_parse_arm_time_formats() {
    assert_eq!(try_parse_duration("3s").unwrap(), Duration::from_secs(3));
    assert_eq!(try_parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
    assert_eq!(try_parse_duration("500ms").unwrap(), Duration::from_millis(500));
    assert_eq!(try_parse_duration("3").unwrap(), Duration::from_secs(3));
    assert!(try_parse_duration("abc").is_err());
    assert!(try_parse_duration("-1s").is_err());
}

#[test]
fn test_length_fraction_to_ticks() {
    // ppqn = 24 → whole note = 96 ticks. Fractions are relative to a whole note.
    assert_eq!(try_parse_length_fraction("1/1", 24).unwrap(), 96);  // whole note
    assert_eq!(try_parse_length_fraction("2/1", 24).unwrap(), 192); // two whole notes
    assert_eq!(try_parse_length_fraction("4/1", 24).unwrap(), 384); // four whole notes
    assert_eq!(try_parse_length_fraction("1/4", 24).unwrap(), 24);  // quarter note
    assert_eq!(try_parse_length_fraction("1/8", 24).unwrap(), 12);  // eighth note
    assert_eq!(try_parse_length_fraction("1/12", 24).unwrap(), 8);  // eighth triplet
    assert_eq!(try_parse_length_fraction(" 1 / 8 ", 24).unwrap(), 12); // whitespace tolerant

    // Errors
    assert!(try_parse_length_fraction("1/0", 24).is_err());  // zero denominator
    assert!(try_parse_length_fraction("abc", 24).is_err());  // no slash
    assert!(try_parse_length_fraction("a/b", 24).is_err());  // non-numeric
    assert!(try_parse_length_fraction("1/96", 24).is_err()); // resolves to zero ticks
}

#[test]
fn test_length_enum_to_ticks() {
    // Raw tick counts pass through unchanged regardless of ppqn.
    assert_eq!(Length::Ticks(96).to_ticks(24).unwrap(), 96);
    assert_eq!(Length::Ticks(384).to_ticks(48).unwrap(), 384);
    // Fraction variant resolves against ppqn.
    assert_eq!(Length::Fraction("1/8".to_string()).to_ticks(24).unwrap(), 12);
}

#[test]
fn test_compute_max_tick() {
    // base = max_tracking_beats(16) * ppqn(24) = 384
    let base = 384;
    // Legacy lengths all divide the base → period stays 384.
    assert_eq!(compute_max_tick(base, [96, 192, 384]), 384);
    // A track longer than the base extends the master period to contain it.
    assert_eq!(compute_max_tick(base, [768]), 768);   // "8/1"
    assert_eq!(compute_max_tick(base, [1536]), 1536); // "16/1"
    // Mixed long + short → LCM that holds whole loops of each.
    assert_eq!(compute_max_tick(base, [768, 96]), 768);
    // No tracks → just the base.
    assert_eq!(compute_max_tick(base, []), 384);
    // Zero-length entries are ignored; zero base falls back to 1.
    assert_eq!(compute_max_tick(0, [0]), 1);
}

#[test]
fn test_scene_switch_rebuilds_drum_mapping() {
    fn scene_with_drums(drums: Option<DrumsConfig>) -> SceneConfig {
        SceneConfig {
            name: None,
            bpm: 120,
            metronome_on: false,
            time_signature_numerator: 4,
            time_signature_denominator: 4,
            ppqn: 24,
            max_tracking_beats: 16,
            tracks: HashMap::new(),
            drums,
        }
    }

    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    let keys = test_keys();
    {
        let mut k = keys.write().unwrap();
        k.scene_down = Some(parse_key("CC90"));
        // Scene A's drum mapping, as built at startup
        k.drum_notes = vec![vec![36]];
        k.drum_channel = 9;
    }

    let scene_a = scene_with_drums(Some(DrumsConfig {
        notes: vec!["36".to_string()],
        channel: 9,
        program: None,
        volume: None,
        velocity: None,
        velocity_multiplier: None,
        length: None,
        name: None,
    }));
    let scene_b = scene_with_drums(Some(DrumsConfig {
        notes: vec!["38+42".to_string()],
        channel: 8,
        program: None,
        volume: None,
        velocity: None,
        velocity_multiplier: None,
        length: None,
        name: None,
    }));
    let scenes = Arc::new(RwLock::new(vec![
        ("a".to_string(), scene_a),
        ("b".to_string(), scene_b),
    ]));

    // Press scene_down → switch to scene B; pads must now play scene B's drums
    let msg = [0xB0, 90, 127];
    handle_midi_message(&msg, &state, &midi_out, &led_out, &keys, &scenes);
    assert_eq!(state.lock().unwrap().current_scene_idx, 1);
    {
        let k = keys.read().unwrap();
        assert_eq!(k.drum_notes, vec![vec![38, 42]]);
        assert_eq!(k.drum_channel, 8);
    }

    // Press again → wraps back to scene A; mapping restored
    handle_midi_message(&msg, &state, &midi_out, &led_out, &keys, &scenes);
    assert_eq!(state.lock().unwrap().current_scene_idx, 0);
    {
        let k = keys.read().unwrap();
        assert_eq!(k.drum_notes, vec![vec![36]]);
        assert_eq!(k.drum_channel, 9);
    }
}

#[test]
fn test_record_arm_sets_armed_timestamp() {
    let state = setup_test_state();
    let (midi_out, led_out) = null_ports();

    // CC 77 press arms recording and stamps the arm time
    let msg = [0xB0, 77, 127];
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    {
        let s = state.lock().unwrap();
        assert!(s.pending_record);
        assert!(s.record_armed_at.is_some());
    }

    // Second press cancels the armed state and clears the timestamp
    handle_midi_message(&msg, &state, &midi_out, &led_out, &test_keys(), &empty_scenes());
    {
        let s = state.lock().unwrap();
        assert!(!s.pending_record);
        assert!(s.record_armed_at.is_none());
    }
}
