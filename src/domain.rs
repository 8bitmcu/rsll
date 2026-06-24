use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub fn quantize_tick(tick: usize, ticks_per_sixteenth: usize, loop_length: usize) -> usize {
    ((tick + ticks_per_sixteenth / 2) / ticks_per_sixteenth * ticks_per_sixteenth) % loop_length
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MessageType {
    NoteOn,
    NoteOff,
    PitchBend,
}

#[derive(Clone, Debug)]
pub struct TrackEvent {
    pub tick: usize,
    pub message_type: MessageType,
    pub data1: u8, // NoteOn/NoteOff: note number; PitchBend: LSB
    pub data2: u8, // NoteOn/NoteOff: velocity;    PitchBend: MSB
    pub channel: u8,
}

pub struct EngineState {
    pub bpm: u32,
    pub ppqn: usize,
    pub time_signature_numerator: u32,
    pub time_signature_denominator: u32,
    pub ticks_per_measure: usize,
    pub max_tick: usize,
    pub current_tick: usize,
    pub is_recording: bool,
    pub pending_record: bool,
    /// When record was armed; punch-in waits until `record_arm_time` has elapsed
    pub record_armed_at: Option<Instant>,
    /// Minimum time between arming and punch-in ([record_track] arm_time)
    pub record_arm_time: Duration,
    pub metronome_on: bool,
    pub quantize_on: bool,
    pub active_track: usize,
    pub track_channels: HashMap<usize, u8>,
    pub tracks: HashMap<usize, Vec<TrackEvent>>,
    pub track_lengths: HashMap<usize, usize>,
    pub track_programs: HashMap<usize, u8>,
    pub track_volumes: HashMap<usize, u8>,
    pub track_velocities: HashMap<usize, Option<u8>>,
    pub track_velocity_multipliers: HashMap<usize, f32>,
    pub drum_velocity: Option<u8>,
    pub drum_velocity_multiplier: f32,
    pub track_history: HashMap<usize, Vec<Vec<TrackEvent>>>,
    pub muted_tracks: HashSet<usize>,
    pub loop_start_lens: HashMap<usize, usize>,
    /// Notes physically held down per track while recording, as (note, channel).
    /// Used to auto-seal a NoteOff at the loop boundary or on record stop so a
    /// held key doesn't leave a dangling NoteOn that re-triggers forever (#84).
    pub held_notes: HashMap<usize, HashSet<(u8, u8)>>,
    /// Keys physically down right now, as (note, channel), tracked regardless of
    /// recording state. A note pressed just before punch-in is re-armed at the
    /// downbeat from this set so it loops cleanly (#84). Unlike held_notes this is
    /// NOT cleared on scene switch / record stop — it mirrors real key state and is
    /// only updated on the actual NoteOn/NoteOff passthrough.
    pub physically_held: HashSet<(u8, u8)>,
    pub shift_held: bool,
    pub current_scene_idx: usize,
    pub scene_name: String,
    pub scene_display_name: Option<String>,
    pub track_ids: Vec<usize>,
    pub track_names: HashMap<usize, String>,
    pub drums_name: Option<String>,
}
