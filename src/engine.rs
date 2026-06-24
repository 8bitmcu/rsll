use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use std::thread;
use midir::{MidiIO, MidiOutputConnection};
use regex::Regex;

use crate::config::{try_scene_drum_mapping, KeyMappings, MetronomeHwConfig, SceneConfig};
use crate::domain::{EngineState, MessageType, TrackEvent, quantize_tick};

// -----------------------------------------------------------------------------
// MIDI Port Discovery
// -----------------------------------------------------------------------------
pub fn find_port_by_regex<T: MidiIO>(midi_io: &T, pattern: &str) -> Option<(T::Port, String)> {
    let re = Regex::new(pattern).ok()?;
    for port in midi_io.ports() {
        if let Ok(name) = midi_io.port_name(&port) {
            if re.is_match(&name) {
                return Some((port, name));
            }
        }
    }
    None
}

pub fn find_port_by_any_regex<T: MidiIO>(midi_io: &T, patterns: &[String]) -> Option<(T::Port, String)> {
    for pattern in patterns {
        if let Some(result) = find_port_by_regex(midi_io, pattern) {
            return Some(result);
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Patch Loader
// -----------------------------------------------------------------------------
pub fn load_patches(conn: &mut MidiOutputConnection, config: &SceneConfig) {
    for (track_str, track_cfg) in &config.tracks {
        if let Ok(_) = track_str.parse::<usize>() {
            let ch = track_cfg.channel & 0x0F;
            let vol = track_cfg.volume.unwrap_or(127);
            let _ = conn.send(&[0xB0 | ch, 7, vol]);
            // Program change status byte format: 0xC0 | MIDI_CHANNEL
            let _ = conn.send(&[0xC0 | ch, track_cfg.program]);
        }
    }
    if let Some(ref drums) = config.drums {
        let ch = drums.channel & 0x0F;
        let vol = drums.volume.unwrap_or(127);
        let _ = conn.send(&[0xB0 | ch, 7, vol]);
        if let Some(program) = drums.program {
            let _ = conn.send(&[0xC0 | ch, program]);
        }
    }
}

/// Greatest common divisor (Euclid).
fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Master-clock loop period in ticks. Starts from `base` (`max_tracking_beats *
/// ppqn`) and extends — via least-common-multiple — to a whole multiple of every
/// track loop length, so loops longer than the base (e.g. `length = "8/1"`)
/// complete their full cycle instead of being truncated when the global tick
/// counter wraps.
pub fn compute_max_tick(base: usize, track_lengths: impl IntoIterator<Item = usize>) -> usize {
    let mut acc = base.max(1);
    for len in track_lengths {
        if len > 0 {
            acc = acc / gcd(acc, len) * len;
        }
    }
    acc
}

// -----------------------------------------------------------------------------
// Scene Switching
// -----------------------------------------------------------------------------
pub fn switch_scene(
    state: &Arc<Mutex<EngineState>>,
    midi_out: &Arc<Mutex<Option<MidiOutputConnection>>>,
    scenes: &[(String, SceneConfig)],
    new_idx: usize,
) {
    if scenes.is_empty() { return; }
    let (name, scene) = &scenes[new_idx];
    let ticks_per_measure = scene.ppqn * scene.time_signature_numerator as usize;
    let max_track_id = scene.tracks.keys()
        .filter_map(|k| k.parse::<usize>().ok())
        .max()
        .unwrap_or(1);

    {
        let mut st = state.lock().unwrap();
        st.current_scene_idx = new_idx;
        st.scene_name = name.clone();
        st.scene_display_name = scene.name.clone();
        st.bpm = scene.bpm;
        st.time_signature_numerator = scene.time_signature_numerator;
        st.time_signature_denominator = scene.time_signature_denominator;
        st.ticks_per_measure = ticks_per_measure;
        st.metronome_on = scene.metronome_on;
        st.active_track = 1;
        st.is_recording = false;
        st.pending_record = false;
        st.record_armed_at = None;
        st.current_tick = 0;
        st.tracks.clear();
        st.track_channels.clear();
        st.track_lengths.clear();
        st.track_programs.clear();
        st.track_volumes.clear();
        st.track_velocities.clear();
        st.track_velocity_multipliers.clear();
        st.track_history.clear();
        st.muted_tracks.clear();
        st.loop_start_lens.clear();
        st.held_notes.clear();
        st.track_ids.clear();
        st.track_names.clear();
        st.drums_name = None;

        for i in 1..=max_track_id {
            let key = i.to_string();
            if let Some(t_cfg) = scene.tracks.get(&key) {
                st.track_channels.insert(i, t_cfg.channel);
                st.track_lengths.insert(i, t_cfg.length.to_ticks(scene.ppqn).unwrap_or(96));
                st.track_programs.insert(i, t_cfg.program);
                st.track_volumes.insert(i, t_cfg.volume.unwrap_or(127));
                st.track_velocities.insert(i, t_cfg.velocity);
                st.track_velocity_multipliers.insert(i, t_cfg.velocity_multiplier.unwrap_or(1.0));
                if let Some(ref name) = t_cfg.name {
                    st.track_names.insert(i, name.clone());
                }
            } else {
                st.track_channels.insert(i, 0);
                st.track_lengths.insert(i, 96);
                st.track_programs.insert(i, 0);
                st.track_volumes.insert(i, 127);
                st.track_velocities.insert(i, None);
                st.track_velocity_multipliers.insert(i, 1.0);
            }
            st.tracks.insert(i, Vec::new());
            st.track_ids.push(i);
        }

        if let Some(ref drums) = scene.drums {
            st.track_channels.insert(0, drums.channel);
            st.track_lengths.insert(0, drums.length.as_ref()
                .map(|l| l.to_ticks(scene.ppqn).unwrap_or(ticks_per_measure))
                .unwrap_or(ticks_per_measure));
            st.tracks.insert(0, Vec::new());
            st.drum_velocity = drums.velocity;
            st.drum_velocity_multiplier = drums.velocity_multiplier.unwrap_or(1.0);
            st.drums_name = drums.name.clone();
        } else {
            st.tracks.remove(&0);
            st.track_channels.remove(&0);
            st.track_lengths.remove(&0);
            st.drum_velocity = None;
            st.drum_velocity_multiplier = 1.0;
            st.drums_name = None;
        }

        let max_tick = compute_max_tick(
            scene.max_tracking_beats * scene.ppqn,
            st.track_lengths.values().copied(),
        );
        st.max_tick = max_tick;
    }

    if let Some(ref mut conn) = *midi_out.lock().unwrap() {
        load_patches(conn, scene);
    }
}

// -----------------------------------------------------------------------------
// Scene Reload (diff-based, preserves track recordings where possible)
// -----------------------------------------------------------------------------
/// Apply changes from a reloaded scene config to the currently active scene
/// without wiping track recordings unless structurally necessary (e.g. length change).
pub fn apply_scene_reload(
    state: &Arc<Mutex<EngineState>>,
    midi_out: &Arc<Mutex<Option<MidiOutputConnection>>>,
    new_scene: &SceneConfig,
) {
    let ticks_per_measure = new_scene.ppqn * new_scene.time_signature_numerator as usize;
    let new_max_track_id = new_scene.tracks.keys()
        .filter_map(|k| k.parse::<usize>().ok())
        .max()
        .unwrap_or(1);

    {
        let mut st = state.lock().unwrap();

        // Check if structural timing params changed — forces full reset
        let timing_changed = st.ppqn != new_scene.ppqn
            || st.ticks_per_measure != ticks_per_measure;

        // Always safe to hot-swap these
        st.bpm = new_scene.bpm;
        st.metronome_on = new_scene.metronome_on;
        st.ppqn = new_scene.ppqn;
        st.time_signature_numerator = new_scene.time_signature_numerator;
        st.time_signature_denominator = new_scene.time_signature_denominator;
        st.ticks_per_measure = ticks_per_measure;
        st.scene_display_name = new_scene.name.clone();

        if timing_changed {
            // Timing structure changed — must reset everything
            st.current_tick = 0;
            st.tracks.clear();
            st.track_channels.clear();
            st.track_lengths.clear();
            st.track_programs.clear();
            st.track_volumes.clear();
            st.track_velocities.clear();
            st.track_velocity_multipliers.clear();
            st.track_history.clear();
            st.muted_tracks.clear();
            st.track_ids.clear();
            st.track_names.clear();

            for i in 1..=new_max_track_id {
                let key = i.to_string();
                if let Some(t_cfg) = new_scene.tracks.get(&key) {
                    st.track_channels.insert(i, t_cfg.channel);
                    st.track_lengths.insert(i, t_cfg.length.to_ticks(new_scene.ppqn).unwrap_or(96));
                    st.track_programs.insert(i, t_cfg.program);
                    st.track_volumes.insert(i, t_cfg.volume.unwrap_or(127));
                    st.track_velocities.insert(i, t_cfg.velocity);
                    st.track_velocity_multipliers.insert(i, t_cfg.velocity_multiplier.unwrap_or(1.0));
                    if let Some(ref name) = t_cfg.name {
                        st.track_names.insert(i, name.clone());
                    }
                } else {
                    st.track_channels.insert(i, 0);
                    st.track_lengths.insert(i, 96);
                    st.track_programs.insert(i, 0);
                    st.track_volumes.insert(i, 127);
                    st.track_velocities.insert(i, None);
                    st.track_velocity_multipliers.insert(i, 1.0);
                }
                st.tracks.insert(i, Vec::new());
                st.track_ids.push(i);
            }
        } else {
            // Timing preserved — diff per-track
            let old_track_ids = st.track_ids.clone();
            let mut new_track_ids = Vec::new();

            for i in 1..=new_max_track_id {
                let key = i.to_string();
                let (new_ch, new_len, new_prog, new_vol, new_vel_override, new_vel) = if let Some(t_cfg) = new_scene.tracks.get(&key) {
                    (t_cfg.channel, t_cfg.length.to_ticks(new_scene.ppqn).unwrap_or(96), t_cfg.program, t_cfg.volume.unwrap_or(127), t_cfg.velocity, t_cfg.velocity_multiplier.unwrap_or(1.0))
                } else {
                    (0, 96, 0, 127u8, None, 1.0f32)
                };

                let old_len = st.track_lengths.get(&i).copied();
                let length_changed = old_len.map_or(true, |l| l != new_len);

                st.track_channels.insert(i, new_ch);
                st.track_lengths.insert(i, new_len);
                st.track_programs.insert(i, new_prog);
                st.track_volumes.insert(i, new_vol);
                st.track_velocities.insert(i, new_vel_override);
                st.track_velocity_multipliers.insert(i, new_vel);

                // Update track name
                let new_name = new_scene.tracks.get(&key).and_then(|t| t.name.clone());
                if let Some(n) = new_name {
                    st.track_names.insert(i, n);
                } else {
                    st.track_names.remove(&i);
                }

                if length_changed {
                    // Track length changed or is new — wipe recordings
                    st.tracks.insert(i, Vec::new());
                    st.track_history.remove(&i);
                } else if !st.tracks.contains_key(&i) {
                    st.tracks.insert(i, Vec::new());
                }
                new_track_ids.push(i);
            }

            // Remove tracks that no longer exist in the new config
            for old_id in &old_track_ids {
                if *old_id > new_max_track_id {
                    st.tracks.remove(old_id);
                    st.track_channels.remove(old_id);
                    st.track_lengths.remove(old_id);
                    st.track_programs.remove(old_id);
                    st.track_volumes.remove(old_id);
                    st.track_velocities.remove(old_id);
                    st.track_velocity_multipliers.remove(old_id);
                    st.track_history.remove(old_id);
                    st.muted_tracks.remove(old_id);
                    st.track_names.remove(old_id);
                }
            }
            st.track_ids = new_track_ids;

            // Clamp active_track if it went out of range
            if !st.track_channels.contains_key(&st.active_track) {
                st.active_track = 1;
            }
        }

        // Handle drums
        if let Some(ref drums) = new_scene.drums {
            let new_drum_len = drums.length.as_ref()
                .map(|l| l.to_ticks(new_scene.ppqn).unwrap_or(ticks_per_measure))
                .unwrap_or(ticks_per_measure);
            let old_drum_len = st.track_lengths.get(&0).copied();
            st.track_channels.insert(0, drums.channel);
            st.track_lengths.insert(0, new_drum_len);
            st.drum_velocity = drums.velocity;
            st.drum_velocity_multiplier = drums.velocity_multiplier.unwrap_or(1.0);
            st.drums_name = drums.name.clone();
            if old_drum_len.map_or(true, |l| l != new_drum_len) {
                st.tracks.insert(0, Vec::new());
            } else if !st.tracks.contains_key(&0) {
                st.tracks.insert(0, Vec::new());
            }
        } else {
            st.tracks.remove(&0);
            st.track_channels.remove(&0);
            st.track_lengths.remove(&0);
            st.drum_velocity = None;
            st.drum_velocity_multiplier = 1.0;
            st.drums_name = None;
        }

        let max_tick = compute_max_tick(
            new_scene.max_tracking_beats * new_scene.ppqn,
            st.track_lengths.values().copied(),
        );
        st.max_tick = max_tick;
    }

    // Send updated patches to the synth
    if let Some(ref mut conn) = *midi_out.lock().unwrap() {
        load_patches(conn, new_scene);
    }
}

// -----------------------------------------------------------------------------
// LED Feedback
// -----------------------------------------------------------------------------
pub fn send_led(led_out: &Arc<Mutex<Option<MidiOutputConnection>>>, note: u8, channel: u8) {
    if let Some(ref mut conn) = *led_out.lock().unwrap() {
        let _ = conn.send(&[0x90 | channel, note, 127]);
    }
}

// -----------------------------------------------------------------------------
// MIDI Stream Message Parsing Matrix
// -----------------------------------------------------------------------------
pub fn handle_midi_message(
    bytes: &[u8],
    state: &Arc<Mutex<EngineState>>,
    midi_out: &Arc<Mutex<Option<MidiOutputConnection>>>,
    led_out: &Arc<Mutex<Option<MidiOutputConnection>>>,
    keys: &Arc<RwLock<KeyMappings>>,
    scenes: &Arc<RwLock<Vec<(String, SceneConfig)>>>,
) {
    if bytes.is_empty() {
        return;
    }
    let status = bytes[0];
    let msg_type = status & 0xF0;
    let channel = status & 0x0F; // 0-indexed: 0 = Ch 1, 9 = Ch 10 (Drums/Pads)

    // Acquire read lock on key mappings once per message
    let keys_arc = keys;
    let keys = keys.read().unwrap();

    // =========================================================================
    // 1. NOTE MESSAGE HANDLER MATRIX (0x90 / 0x80)
    // =========================================================================
    if (msg_type == 0x90 || msg_type == 0x80) && bytes.len() >= 3 {
        let note = bytes[1];
        let velocity = bytes[2];
        let mut actual_msg_type = msg_type;

        if msg_type == 0x90 && velocity == 0 {
            actual_msg_type = 0x80;
        }

        let mut state = state.lock().unwrap();
        let active_track = state.active_track;

        // --- EXCLUSIVE SHIFT MATRIX ---
        // If the shift state is held down, intercept notes on the shift modifier's channel
        if state.shift_held && keys.shift.channel.map_or(true, |ch| ch == channel) {
            if actual_msg_type == 0x90 {
                let new_length: Option<usize> =
                    if keys.beat4.matches_note(note, channel) { Some(96) }
                    else if keys.beat8.matches_note(note, channel) { Some(192) }
                    else if keys.beat16.matches_note(note, channel) { Some(384) }
                    else { None };

                if let Some(len) = new_length {
                    if let Some(track) = state.tracks.get_mut(&active_track) {
                        track.retain(|e| e.tick < len);
                    }
                    state.track_lengths.insert(active_track, len);
                }
            }
            // Strict firewall return: swallow notes on shift modifier's channel while shift is active
            return;
        }

        // --- STANDARD PERFORMANCE & ROUTING ENGINE ---
        // Pads (typically on Ch 10 / channel == 9) and non-shifted instrument notes drop down here
        if channel == 9 {
            // If you want your pads to play live drum kit notes into FluidSynth, pass them along.
            // Otherwise, if pads are strictly for control macros, you can 'return;' here.
            return;
        }

        let target_channel = state.track_channels[&active_track];
        let mut outbound_bytes = bytes.to_vec();

        // Re-route the performance message to the active track's synth channel
        outbound_bytes[0] = actual_msg_type | target_channel;

        // Apply velocity override + multiplier to live NoteOn send (NoteOff / vel=0 passthrough unchanged).
        // The `velocity` field replaces the incoming velocity, then `velocity_multiplier` scales the result.
        if actual_msg_type == 0x90 && outbound_bytes[2] > 0 {
            let vel_override = state.track_velocities.get(&active_track).copied().flatten();
            let vel_mult = state.track_velocity_multipliers.get(&active_track).copied().unwrap_or(1.0);
            let base = vel_override.unwrap_or(outbound_bytes[2]);
            outbound_bytes[2] = ((base as f32 * vel_mult) as u32).min(127) as u8;
        }

        // Track physically-held keys regardless of recording state so a note pressed
        // just before punch-in can be re-armed at the downbeat (#84). Updated on the
        // passthrough, outside the is_recording gate.
        if actual_msg_type == 0x90 {
            state.physically_held.insert((note, target_channel));
        } else {
            state.physically_held.remove(&(note, target_channel));
        }

        if state.is_recording {
            let track_length = state.track_lengths[&active_track];
            let recorded_tick = if state.quantize_on {
                quantize_tick(state.current_tick, state.ppqn / 4, track_length)
            } else {
                state.current_tick % track_length
            };
            if actual_msg_type == 0x90 {
                state.tracks.get_mut(&active_track).unwrap().push(TrackEvent {
                    tick: recorded_tick,
                    message_type: MessageType::NoteOn,
                    data1: note,
                    data2: velocity,
                    channel: target_channel,
                });
                // Track physically-held notes so a still-held key can be auto-sealed
                // with a NoteOff at the loop boundary or on record stop (#84).
                state.held_notes.entry(active_track).or_default().insert((note, target_channel));
            } else {
                // Only record a NoteOff for a note still in held_notes. If the note was
                // already sealed at a loop boundary (seal-at-wrap removed it), it now
                // "belongs to the loop" as a full-length drone — a later physical release
                // must NOT write a truncating NoteOff that would clobber the sustain (#84).
                // The live NoteOff still passes through below to stop the physical sound.
                let was_held = state.held_notes.get_mut(&active_track)
                    .map_or(false, |held| held.remove(&(note, target_channel)));
                if was_held {
                    state.tracks.get_mut(&active_track).unwrap().push(TrackEvent {
                        tick: recorded_tick,
                        message_type: MessageType::NoteOff,
                        data1: note,
                        data2: velocity,
                        channel: target_channel,
                    });
                }
            }
        }

        if let Some(ref mut conn) = *midi_out.lock().unwrap() {
            let _ = conn.send(&outbound_bytes);
        }
    }

    // =========================================================================
    // 2. CONTROL CHANGE HANDLER MATRIX (0xB0)
    // =========================================================================
    else if msg_type == 0xB0 && bytes.len() >= 3 {
        let cc_num = bytes[1];
        let value = bytes[2];

        // Catch the momentary release of the shift modifier key on its designated channel
        if keys.shift.matches_cc(cc_num, channel) && value == 0 {
            let mut state = state.lock().unwrap();
            state.shift_held = false;

            // Panic sweep to clear notes held down on the active track during the config shift window
            let target_channel = state.track_channels[&state.active_track];
            let panic_msg = [0xB0 | target_channel, 123, 0];
            if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                let _ = conn.send(&panic_msg);
            }
            return;
        }

        // Drum pad handler — intercept before the generic value==0 guard so releases fire NoteOff
        if let Some(pad_idx) = keys.drum_keys.iter().position(|k| k.matches_cc(cc_num, channel)) {
            if let Some(notes) = keys.drum_notes.get(pad_idx) {
                let status = if value > 0 { 0x90 } else { 0x80 };
                let effective_value = if value > 0 {
                    let st = state.lock().unwrap();
                    let base = st.drum_velocity.unwrap_or(value);
                    ((base as f32 * st.drum_velocity_multiplier) as u32).min(127) as u8
                } else {
                    0
                };
                if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                    for &note in notes {
                        let _ = conn.send(&[status | keys.drum_channel, note, effective_value]);
                    }
                }
                // Record to the drums lane (track 0) when recording is active
                let mut st = state.lock().unwrap();
                if st.is_recording {
                    let track_len = st.track_lengths.get(&0).copied().unwrap_or(st.ticks_per_measure);
                    let tick = if st.quantize_on {
                        quantize_tick(st.current_tick, st.ppqn / 4, track_len)
                    } else {
                        st.current_tick % track_len
                    };
                    // Record drum events. A NoteOff is only written for a pad note still
                    // in held_notes; if it was already sealed at a loop boundary it's a
                    // full-loop drone and a later release must not truncate it (#84). Held
                    // drum notes are also tracked so seal-at-wrap can close them.
                    for &note in notes {
                        if value > 0 {
                            if let Some(drum_track) = st.tracks.get_mut(&0) {
                                drum_track.push(TrackEvent {
                                    tick,
                                    message_type: MessageType::NoteOn,
                                    data1: note,
                                    data2: effective_value,
                                    channel: keys.drum_channel,
                                });
                            }
                            st.held_notes.entry(0).or_default().insert((note, keys.drum_channel));
                        } else {
                            let was_held = st.held_notes.get_mut(&0)
                                .map_or(false, |held| held.remove(&(note, keys.drum_channel)));
                            if was_held {
                                if let Some(drum_track) = st.tracks.get_mut(&0) {
                                    drum_track.push(TrackEvent {
                                        tick,
                                        message_type: MessageType::NoteOff,
                                        data1: note,
                                        data2: effective_value,
                                        channel: keys.drum_channel,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            return;
        }

        // Instrument select — must come before value==0 guard so program 0 (Grand Piano) works
        if let Some(ref inst_key) = keys.instrument_select {
            if inst_key.matches_cc(cc_num, channel) {
                let target_channel = {
                    let mut st = state.lock().unwrap();
                    let at = st.active_track;
                    st.track_programs.insert(at, value);
                    st.track_channels[&at]
                };
                if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                    let _ = conn.send(&[0xC0 | target_channel, value]);
                }
                return;
            }
        }

        // Volume knob — send CC7 (channel volume) to the active track's synth channel.
        // Handled before value==0 guard so value=0 (silence) is valid.
        if let Some(ref vol_key) = keys.volume_knob {
            if vol_key.matches_cc(cc_num, channel) {
                let (target_channel, active_track) = {
                    let st = state.lock().unwrap();
                    (st.track_channels[&st.active_track], st.active_track)
                };
                state.lock().unwrap().track_volumes.insert(active_track, value);
                if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                    let _ = conn.send(&[0xB0 | target_channel, 7, value]);
                }
                return;
            }
        }

        // BPM knob — maps 0-127 → 40-167 BPM. Handled before value==0 guard.
        if let Some(ref bpm_key) = keys.bpm_knob {
            if bpm_key.matches_cc(cc_num, channel) {
                state.lock().unwrap().bpm = 40 + value as u32;
                return;
            }
        }

        // Scene navigation — handled before value==0 guard with explicit press check.
        if value > 0 {
            let scene_down = keys.scene_down.as_ref().map_or(false, |k| k.matches_cc(cc_num, channel));
            let scene_up = !scene_down && keys.scene_up.as_ref().map_or(false, |k| k.matches_cc(cc_num, channel));
            if scene_down || scene_up {
                // Release the read lock so the drum mapping can be rebuilt below
                drop(keys);
                let scenes_guard = scenes.read().unwrap();
                let count = scenes_guard.len();
                if count > 0 {
                    let cur = state.lock().unwrap().current_scene_idx;
                    let new_idx = if scene_down { (cur + 1) % count } else { (cur + count - 1) % count };
                    switch_scene(state, midi_out, &scenes_guard, new_idx);
                    // Rebuild scene-specific drum pad mapping so pads play the new scene's drums
                    match try_scene_drum_mapping(&scenes_guard[new_idx].1) {
                        Ok((drum_notes, drum_channel)) => {
                            let mut k = keys_arc.write().unwrap();
                            k.drum_notes = drum_notes;
                            k.drum_channel = drum_channel;
                        }
                        Err(e) => eprintln!("[engine] failed to rebuild drum mapping: {}", e),
                    }
                }
                return;
            }
        }

        if value == 0 {
            return; // Ignore all other generic utility pad release triggers safely
        }

        let mut state = state.lock().unwrap();
        let active_track = state.active_track;

        match cc_num {
            // Catch the momentary press of the shift modifier on its designated channel
            _ if keys.shift.matches_cc(cc_num, channel) => {
                state.shift_held = true;
            }
            _ if keys.metronome.matches_cc(cc_num, channel) => {
                state.metronome_on = !state.metronome_on;
                if let (Some(note), Some(on_ch), Some(off_ch)) = (keys.metronome_led, keys.led_on_channel, keys.led_off_channel) {
                    let ch = if state.metronome_on { on_ch } else { off_ch };
                    drop(state);
                    send_led(led_out, note, ch);
                }
            }
            _ if keys.clear_track.matches_cc(cc_num, channel) => {
                if let Some(track) = state.tracks.get_mut(&active_track) {
                    track.clear();
                }
                state.held_notes.remove(&active_track);
                let target_channel = state.track_channels[&active_track];
                let panic_msg = [0xB0 | target_channel, 123, 0];
                if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                    let _ = conn.send(&panic_msg);
                }
            }
            _ if keys.clear_all.matches_cc(cc_num, channel) => {
                for track in state.tracks.values_mut() {
                    track.clear();
                }
                state.held_notes.clear();
                let mut unique_channels: Vec<u8> = state.track_channels.values().cloned().collect();
                unique_channels.sort();
                unique_channels.dedup();
                if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                    for &ch in &unique_channels {
                        let panic_msg = [0xB0 | ch, 123, 0];
                        let _ = conn.send(&panic_msg);
                    }
                }
            }
            _ if keys.record_track.matches_cc(cc_num, channel) => {
                if state.is_recording {
                    // Stop recording immediately
                    state.is_recording = false;
                    // Seal any still-held notes with a NoteOff at the current tick so a
                    // key held when recording stops doesn't leave a dangling NoteOn (#84).
                    let current_tick = state.current_tick;
                    let quantize_on = state.quantize_on;
                    let ppqn = state.ppqn;
                    let ticks_per_measure = state.ticks_per_measure;
                    let mut seals: Vec<(usize, usize, u8, u8)> = Vec::new();
                    for (&track_id, held) in &state.held_notes {
                        let track_len = state.track_lengths.get(&track_id).copied().unwrap_or(ticks_per_measure);
                        let tick = if quantize_on {
                            quantize_tick(current_tick, ppqn / 4, track_len)
                        } else {
                            current_tick % track_len
                        };
                        for &(note, ch) in held {
                            seals.push((track_id, tick, note, ch));
                        }
                    }
                    for (track_id, tick, note, ch) in seals {
                        if let Some(track) = state.tracks.get_mut(&track_id) {
                            track.push(TrackEvent {
                                tick,
                                message_type: MessageType::NoteOff,
                                data1: note,
                                data2: 0,
                                channel: ch,
                            });
                        }
                    }
                    state.held_notes.clear();
                    if let (Some(note), Some(off_ch)) = (keys.record_led, keys.led_off_channel) {
                        drop(state);
                        send_led(led_out, note, off_ch);
                    }
                } else if state.pending_record {
                    // Cancel armed state
                    state.pending_record = false;
                    state.record_armed_at = None;
                    state.held_notes.clear();
                    if let (Some(note), Some(off_ch)) = (keys.record_led, keys.led_off_channel) {
                        drop(state);
                        send_led(led_out, note, off_ch);
                    }
                } else {
                    // Arm: will punch in at the next downbeat after the minimum
                    // arm time has elapsed — blink LED to signal armed
                    state.pending_record = true;
                    state.record_armed_at = Some(Instant::now());
                    if let (Some(note), Some(blink_ch)) = (keys.record_led, keys.led_blink_channel) {
                        drop(state);
                        send_led(led_out, note, blink_ch);
                    }
                }
            }
            _ if keys.undo_track.as_ref().map_or(false, |k| k.matches_cc(cc_num, channel)) => {
                let active_track = state.active_track;
                if let Some(history) = state.track_history.get_mut(&active_track) {
                    if let Some(previous) = history.pop() {
                        state.tracks.insert(active_track, previous);
                    }
                }
            }
            _ if keys.quantize.as_ref().map_or(false, |k| k.matches_cc(cc_num, channel)) => {
                state.quantize_on = !state.quantize_on;
            }
            _ if keys.mute_track.as_ref().map_or(false, |k| k.matches_cc(cc_num, channel)) => {
                let target_channel = state.track_channels[&active_track];
                if state.muted_tracks.contains(&active_track) {
                    state.muted_tracks.remove(&active_track);
                } else {
                    state.muted_tracks.insert(active_track);
                    // Send all-notes-off so muted notes don't ring out
                    if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                        let _ = conn.send(&[0xB0 | target_channel, 123, 0]);
                    }
                }
            }
            // Track instrument selection pads
            _ => {
                if let Some(idx) = keys.track_keys.iter().position(|k| k.matches_cc(cc_num, channel)) {
                    let track_num = idx + 1;
                    if state.track_channels.contains_key(&track_num) {
                        state.active_track = track_num;
                    }
                }
            }
        }
    }

    // =========================================================================
    // 3. PITCH BEND HANDLER (0xE0)
    // =========================================================================
    else if msg_type == 0xE0 && bytes.len() >= 3 {
        let lsb = bytes[1];
        let msb = bytes[2];

        let mut state = state.lock().unwrap();
        let active_track = state.active_track;
        let target_channel = state.track_channels[&active_track];

        if state.is_recording {
            // Pitch bend is always recorded at the actual tick — quantizing a
            // continuous controller would collapse a whole gesture to one point.
            let track_length = state.track_lengths[&active_track];
            let recorded_tick = state.current_tick % track_length;
            state.tracks.get_mut(&active_track).unwrap().push(TrackEvent {
                tick: recorded_tick,
                message_type: MessageType::PitchBend,
                data1: lsb,
                data2: msb,
                channel: target_channel,
            });
        }

        if let Some(ref mut conn) = *midi_out.lock().unwrap() {
            let _ = conn.send(&[0xE0 | target_channel, lsb, msb]);
        }
    }
}

// -----------------------------------------------------------------------------
// Precision Master Clock Thread Engine
// -----------------------------------------------------------------------------
pub fn run_master_clock(
    state: Arc<Mutex<EngineState>>,
    midi_out: Arc<Mutex<Option<MidiOutputConnection>>>,
    led_out: Arc<Mutex<Option<MidiOutputConnection>>>,
    metro: Arc<RwLock<MetronomeHwConfig>>,
    record_led: Arc<RwLock<Option<u8>>>,
    led_on_channel: Arc<RwLock<Option<u8>>>,
) {
    let ticks_per_beat = {
        let s = state.lock().unwrap();
        s.ppqn
    };
    let mut tick_duration;

    let start_time = Instant::now();
    let mut next_tick_time = start_time;

    loop {
        let mut events_to_play = Vec::new();
        let mut click_to_send = None;
        let mut send_record_on_led = false;

        {
            let mut s = state.lock().unwrap();
            tick_duration = Duration::from_secs_f64(60.0 / (s.bpm as f64 * ticks_per_beat as f64));
            let tick = s.current_tick;

            // Punch in at downbeat when record is armed and the minimum arm time has elapsed
            let arm_time_elapsed = s.record_armed_at
                .map_or(true, |armed_at| armed_at.elapsed() >= s.record_arm_time);
            if s.pending_record && arm_time_elapsed && tick % s.ticks_per_measure == 0 {
                let active_track = s.active_track;
                let snapshot = s.tracks.get(&active_track).cloned().unwrap_or_default();
                s.track_history.entry(active_track).or_default().push(snapshot);
                s.pending_record = false;
                s.record_armed_at = None;
                s.is_recording = true;
                send_record_on_led = true;
                // Baseline the loop snapshot for every track at punch-in. Each track's
                // boundary may not align with the punch-in downbeat (e.g. a 2-measure drum
                // loop punched in on an odd measure), so without this the clock would fall
                // back to replaying the full ledger and double the events recorded this pass.
                let baseline: Vec<(usize, usize)> =
                    s.tracks.iter().map(|(&id, ledger)| (id, ledger.len())).collect();
                for (id, len) in baseline {
                    s.loop_start_lens.insert(id, len);
                }

                // Re-arm any keys already physically held when recording punches in, so a
                // note pressed slightly before the record button starts the loop cleanly at
                // the downbeat (#84). Pushed AFTER the loop_start_lens baseline above so the
                // re-arm NoteOn is treated as a current-pass event: the clock won't replay it
                // this pass (the note is already sounding live) and it overlays from the next
                // loop. Each note is also inserted into held_notes so the existing
                // seal-at-wrap logic closes it at the loop boundary.
                let prearmed: Vec<(u8, u8)> = s.physically_held.iter().copied().collect();
                if !prearmed.is_empty() {
                    if let Some(ledger) = s.tracks.get_mut(&active_track) {
                        for &(note, ch) in &prearmed {
                            ledger.push(TrackEvent {
                                tick: 0,
                                message_type: MessageType::NoteOn,
                                data1: note,
                                data2: 100,
                                channel: ch,
                            });
                        }
                    }
                    let held = s.held_notes.entry(active_track).or_default();
                    for (note, ch) in prearmed {
                        held.insert((note, ch));
                    }
                }
            }

            // At each track's loop boundary while recording, snapshot how many events exist.
            // The clock will only replay events up to this index — events recorded in the
            // current pass are played live by the handler and must not also fire from the clock.
            if s.is_recording {
                let snapshots: Vec<(usize, usize)> = s.tracks.iter()
                    .filter(|(&track_id, _)| {
                        let track_len = s.track_lengths.get(&track_id).copied().unwrap_or(s.ticks_per_measure);
                        tick % track_len == 0
                    })
                    .map(|(&track_id, ledger)| (track_id, ledger.len()))
                    .collect();
                for (track_id, len) in snapshots {
                    s.loop_start_lens.insert(track_id, len);
                }

                // Seal any notes still physically held at each track's loop boundary by
                // recording a NoteOff at the end of the just-finished pass (tick
                // track_len - 1). A continuously-held key thus becomes a clean per-loop
                // retrigger instead of a dangling NoteOn (#84). Pushed AFTER the
                // loop_start_lens snapshot above so the #44 anti-flam machinery treats
                // it as a current-pass event: it won't double this pass and overlays
                // next loop. We don't send a live NoteOff, so the physical note keeps
                // ringing until the player actually releases the key.
                let track_ids: Vec<usize> = s.tracks.keys().copied().collect();
                let mut seal_targets: Vec<(usize, usize, u8, u8)> = Vec::new();
                for track_id in track_ids {
                    let track_len = s.track_lengths.get(&track_id).copied().unwrap_or(s.ticks_per_measure);
                    if tick % track_len == 0 {
                        if let Some(held) = s.held_notes.get(&track_id) {
                            for &(note, ch) in held {
                                seal_targets.push((track_id, track_len, note, ch));
                            }
                        }
                    }
                }
                for (track_id, track_len, note, ch) in seal_targets {
                    if let Some(track) = s.tracks.get_mut(&track_id) {
                        track.push(TrackEvent {
                            tick: track_len - 1,
                            message_type: MessageType::NoteOff,
                            data1: note,
                            data2: 0,
                            channel: ch,
                        });
                    }
                    if let Some(held) = s.held_notes.get_mut(&track_id) {
                        held.remove(&(note, ch));
                    }
                }
            }

            // Playback Engine: Wrap tick calculation around individual track thresholds
            let quantize_on = s.quantize_on;
            let ppqn = s.ppqn;
            let ticks_per_measure = s.ticks_per_measure;
            for (&track_id, ledger) in &s.tracks {
                if s.muted_tracks.contains(&track_id) {
                    continue;
                }
                // During recording, only replay events that existed at the start of this
                // loop pass. Events recorded in the current pass are already played live.
                let events: &[_] = if s.is_recording {
                    let snap = s.loop_start_lens.get(&track_id).copied().unwrap_or(ledger.len());
                    &ledger[..snap.min(ledger.len())]
                } else {
                    ledger
                };
                let track_ticks = s.track_lengths.get(&track_id).copied().unwrap_or(ticks_per_measure);
                // Drum events (track 0) are recorded already-adjusted by the drum handler, so they
                // have no entry here and playback leaves them as-is. Melodic tracks adjust on playback.
                let vel_override = s.track_velocities.get(&track_id).copied().flatten();
                let vel_mult = s.track_velocity_multipliers.get(&track_id).copied().unwrap_or(1.0);
                for event in events {
                    let event_tick = if quantize_on { quantize_tick(event.tick, ppqn / 4, track_ticks) } else { event.tick };
                    if event_tick == tick % track_ticks {
                        let mut ev = event.clone();
                        if ev.message_type == MessageType::NoteOn {
                            let base = vel_override.unwrap_or(ev.data2);
                            ev.data2 = ((base as f32 * vel_mult) as u32).min(127) as u8;
                        }
                        events_to_play.push(ev);
                    }
                }
            }

            // Downbeat tracking for active channel loop reference frame
            if s.metronome_on && (tick % ticks_per_beat == 0) {
                let metro_cfg = metro.read().unwrap();
                let active_len = s.track_lengths.get(&s.active_track).copied().unwrap_or(ticks_per_measure);
                let track_relative_tick = tick % active_len;
                let click_note = if track_relative_tick == 0 { metro_cfg.downbeat_note } else { metro_cfg.subdivision_note };
                click_to_send = Some(click_note);
            }

            s.current_tick = (tick + 1) % s.max_tick.max(1);
        }

        // Switch record LED from blink to solid on at punch-in
        if send_record_on_led {
            let rec_led = *record_led.read().unwrap();
            let on_ch = *led_on_channel.read().unwrap();
            if let (Some(note), Some(ch)) = (rec_led, on_ch) {
                send_led(&led_out, note, ch);
            }
        }

        // Emit playback notes
        if !events_to_play.is_empty() {
            if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                for ev in events_to_play {
                    let status = match ev.message_type {
                        MessageType::NoteOn => 0x90 | ev.channel,
                        MessageType::NoteOff => 0x80 | ev.channel,
                        MessageType::PitchBend => 0xE0 | ev.channel,
                    };
                    let msg = [status, ev.data1, ev.data2];
                    let _ = conn.send(&msg);
                }
            }
        }

        // Emit clock click bytes
        if let Some(click_note) = click_to_send {
            let metro_cfg = metro.read().unwrap();
            if let Some(ref mut conn) = *midi_out.lock().unwrap() {
                let _ = conn.send(&[0x90 | metro_cfg.channel, click_note, metro_cfg.velocity]);
                let _ = conn.send(&[0x80 | metro_cfg.channel, click_note, 0]);
            }
        }

        next_tick_time += tick_duration;
        let now = Instant::now();
        if next_tick_time < now {
            next_tick_time = now;
        } else {
            thread::sleep(next_tick_time - now);
        }
    }
}
