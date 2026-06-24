use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::fmt;
use std::time::Duration;
use serde::Deserialize;

// -----------------------------------------------------------------------------
// TOML Deserialization Structs
// -----------------------------------------------------------------------------

/// Loop length, expressed either as a raw tick count (e.g. `length = 96`) or as a
/// musical fraction string relative to a whole note (e.g. `length = "1/8"`).
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Length {
    Ticks(usize),
    Fraction(String),
}

impl Length {
    /// Resolve to a tick count. Fractions are relative to a whole note
    /// (4 quarter notes = `4 * ppqn` ticks), so `"1/4"` = one quarter note = `ppqn`
    /// ticks, `"1/8"` = `ppqn / 2`, `"1/1"` = a whole note, `"2/1"` = two whole notes.
    pub fn to_ticks(&self, ppqn: usize) -> Result<usize, ReloadError> {
        match self {
            Length::Ticks(t) => Ok(*t),
            Length::Fraction(s) => try_parse_length_fraction(s, ppqn),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct TomlTrackConfig {
    pub channel: u8,
    pub length: Length,
    pub program: u8,
    pub volume: Option<u8>,
    pub velocity: Option<u8>,
    pub velocity_multiplier: Option<f32>,
    pub name: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SceneConfig {
    pub name: Option<String>,
    pub bpm: u32,
    pub metronome_on: bool,
    pub time_signature_numerator: u32,
    pub time_signature_denominator: u32,
    pub ppqn: usize,
    pub max_tracking_beats: usize,
    pub tracks: HashMap<String, TomlTrackConfig>,
    pub drums: Option<DrumsConfig>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InPortConfig {
    pub pattern: String,
    /// 1-indexed MIDI channels to silently drop on this port (e.g. [10] to ignore drum echoes)
    #[serde(default)]
    pub exclude_channels: Vec<u8>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PortsConfig {
    #[serde(rename = "in")]
    pub in_ports: Vec<InPortConfig>,
    pub out: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DeviceConfig {
    pub client_name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MetronomeHwConfig {
    pub key: String,
    pub led_note: Option<u8>,
    pub downbeat_note: u8,
    pub subdivision_note: u8,
    pub velocity: u8,
    pub channel: u8,
}

#[derive(Deserialize, Debug, Clone)]
pub struct HwKeyBinding {
    pub key: String,
    pub led_note: Option<u8>,
    /// Minimum time between arming and punch-in, e.g. "3s" or "500ms" (record_track only)
    pub arm_time: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TracksConfig {
    pub keys: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BeatsPerMeasureConfig {
    pub modifier: String,
    pub key4: String,
    pub key8: String,
    pub key16: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DrumsConfig {
    pub notes: Vec<String>,
    pub channel: u8,
    pub program: Option<u8>,
    pub volume: Option<u8>,
    pub velocity: Option<u8>,
    pub velocity_multiplier: Option<f32>,
    pub length: Option<Length>,
    pub name: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DrumsHwConfig {
    pub pads: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuantizeConfig {
    pub key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InstrumentsConfig {
    pub key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LedConfig {
    pub out: Vec<String>,
    pub on_channel: u8,    // 1-indexed
    pub off_channel: u8,   // 1-indexed
    pub blink_channel: Option<u8>, // 1-indexed; device-level blink mode
}

#[derive(Deserialize, Debug)]
pub struct GmConfig {
    pub programs: HashMap<String, String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct HardwareConfig {
    pub device: DeviceConfig,
    pub ports: PortsConfig,
    pub metronome: MetronomeHwConfig,
    pub clear_track: HwKeyBinding,
    pub record_track: HwKeyBinding,
    pub undo_track: Option<HwKeyBinding>,
    pub clear_all: HwKeyBinding,
    pub tracks: TracksConfig,
    pub beats_per_measure: BeatsPerMeasureConfig,
    pub quantize: Option<QuantizeConfig>,
    pub instruments: Option<InstrumentsConfig>,
    pub leds: Option<LedConfig>,
    pub drums: Option<DrumsHwConfig>,
    pub mute_track: Option<QuantizeConfig>,
    pub volume_knob: Option<QuantizeConfig>,
    pub bpm_knob: Option<QuantizeConfig>,
    pub scene_down: Option<QuantizeConfig>,
    pub scene_up: Option<QuantizeConfig>,
}

// -----------------------------------------------------------------------------
// Key Binding Types
// -----------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq)]
pub enum KeyKind {
    Note(u8),
    Cc(u8),
}

#[derive(Clone, Debug)]
pub struct KeyBinding {
    pub kind: KeyKind,
    pub channel: Option<u8>, // 0-indexed; None means any channel
}

impl KeyBinding {
    pub fn matches_note(&self, note: u8, channel: u8) -> bool {
        self.kind == KeyKind::Note(note)
            && self.channel.map_or(true, |ch| ch == channel)
    }

    pub fn matches_cc(&self, cc_num: u8, channel: u8) -> bool {
        self.kind == KeyKind::Cc(cc_num)
            && self.channel.map_or(true, |ch| ch == channel)
    }
}

/// Parsed key bindings extracted from a HardwareConfig, ready for runtime use.
#[derive(Clone)]
pub struct KeyMappings {
    pub metronome: KeyBinding,
    pub clear_track: KeyBinding,
    pub record_track: KeyBinding,
    pub clear_all: KeyBinding,
    pub track_keys: Vec<KeyBinding>,
    pub shift: KeyBinding,
    pub beat4: KeyBinding,
    pub beat8: KeyBinding,
    pub beat16: KeyBinding,
    pub drum_keys: Vec<KeyBinding>,
    pub drum_notes: Vec<Vec<u8>>,
    pub drum_channel: u8,
    pub metronome_led: Option<u8>,
    pub record_led: Option<u8>,
    pub led_on_channel: Option<u8>,    // 0-indexed
    pub led_off_channel: Option<u8>,   // 0-indexed
    pub led_blink_channel: Option<u8>, // 0-indexed
    pub quantize: Option<KeyBinding>,
    pub instrument_select: Option<KeyBinding>,
    pub undo_track: Option<KeyBinding>,
    pub mute_track: Option<KeyBinding>,
    pub volume_knob: Option<KeyBinding>,
    pub bpm_knob: Option<KeyBinding>,
    pub scene_down: Option<KeyBinding>,
    pub scene_up: Option<KeyBinding>,
}

/// Parse a unified key binding string. Accepted formats:
///   "50"       → note 50, any channel
///   "50_CH1"   → note 50, channel 1 (1-indexed in config, stored 0-indexed)
///   "CC17"     → control change 17, any channel
///   "CC17_CH1" → control change 17, channel 1
pub fn parse_key(s: &str) -> KeyBinding {
    let (base, ch_part) = match s.split_once("_CH") {
        Some((b, ch)) => (b, Some(ch)),
        None => (s, None),
    };
    let channel = ch_part.map(|ch| {
        ch.parse::<u8>()
            .unwrap_or_else(|_| panic!("Invalid channel in key binding: '{}'", s))
            - 1
    });
    if let Some(num_str) = base.strip_prefix("CC") {
        let num = num_str.parse::<u8>()
            .unwrap_or_else(|_| panic!("Invalid CC number in key binding: '{}'", s));
        KeyBinding { kind: KeyKind::Cc(num), channel }
    } else {
        let num = base.parse::<u8>()
            .unwrap_or_else(|_| panic!("Invalid note number in key binding: '{}'", s));
        KeyBinding { kind: KeyKind::Note(num), channel }
    }
}

// -----------------------------------------------------------------------------
// Reload Error Type
// -----------------------------------------------------------------------------
#[derive(Debug)]
pub enum ReloadError {
    Io(std::io::Error),
    Parse(String),
}

impl fmt::Display for ReloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReloadError::Io(e) => write!(f, "IO error: {}", e),
            ReloadError::Parse(msg) => write!(f, "Parse error: {}", msg),
        }
    }
}

impl From<std::io::Error> for ReloadError {
    fn from(e: std::io::Error) -> Self {
        ReloadError::Io(e)
    }
}

/// Fallible version of parse_key for live reload (returns Result instead of panicking).
pub fn try_parse_key(s: &str) -> Result<KeyBinding, ReloadError> {
    let (base, ch_part) = match s.split_once("_CH") {
        Some((b, ch)) => (b, Some(ch)),
        None => (s, None),
    };
    let channel = match ch_part {
        Some(ch) => Some(
            ch.parse::<u8>()
                .map_err(|_| ReloadError::Parse(format!("Invalid channel in key binding: '{}'", s)))?
                .checked_sub(1)
                .ok_or_else(|| ReloadError::Parse(format!("Channel 0 is invalid in key binding: '{}'", s)))?
        ),
        None => None,
    };
    if let Some(num_str) = base.strip_prefix("CC") {
        let num = num_str.parse::<u8>()
            .map_err(|_| ReloadError::Parse(format!("Invalid CC number in key binding: '{}'", s)))?;
        Ok(KeyBinding { kind: KeyKind::Cc(num), channel })
    } else {
        let num = base.parse::<u8>()
            .map_err(|_| ReloadError::Parse(format!("Invalid note number in key binding: '{}'", s)))?;
        Ok(KeyBinding { kind: KeyKind::Note(num), channel })
    }
}

/// Parse a duration string. Accepted formats:
///   "3s"     → 3 seconds
///   "1.5s"   → 1.5 seconds
///   "500ms"  → 500 milliseconds
///   "3"      → 3 seconds (bare number)
pub fn try_parse_duration(s: &str) -> Result<Duration, ReloadError> {
    let trimmed = s.trim();
    let (num_str, is_ms) = if let Some(n) = trimmed.strip_suffix("ms") {
        (n, true)
    } else if let Some(n) = trimmed.strip_suffix('s') {
        (n, false)
    } else {
        (trimmed, false)
    };
    let value: f64 = num_str.trim().parse()
        .map_err(|_| ReloadError::Parse(format!("Invalid duration: '{}'", s)))?;
    if !value.is_finite() || value < 0.0 {
        return Err(ReloadError::Parse(format!("Duration must be non-negative: '{}'", s)));
    }
    let secs = if is_ms { value / 1000.0 } else { value };
    Ok(Duration::from_secs_f64(secs))
}

/// Parse a loop length fraction string into a tick count. The fraction is relative
/// to a whole note (4 quarter notes = `4 * ppqn` ticks). Accepted format: "N/D".
///   "1/1"  → whole note (= 4 * ppqn)
///   "1/4"  → quarter note (= ppqn)
///   "1/8"  → eighth note
///   "1/12" → eighth-note triplet
pub fn try_parse_length_fraction(s: &str, ppqn: usize) -> Result<usize, ReloadError> {
    let (num_str, den_str) = s.trim().split_once('/')
        .ok_or_else(|| ReloadError::Parse(format!("Invalid length fraction (expected 'N/D'): '{}'", s)))?;
    let num: usize = num_str.trim().parse()
        .map_err(|_| ReloadError::Parse(format!("Invalid length numerator: '{}'", s)))?;
    let den: usize = den_str.trim().parse()
        .map_err(|_| ReloadError::Parse(format!("Invalid length denominator: '{}'", s)))?;
    if den == 0 {
        return Err(ReloadError::Parse(format!("Length denominator cannot be zero: '{}'", s)));
    }
    let ticks = (num * ppqn * 4) / den;
    if ticks == 0 {
        return Err(ReloadError::Parse(format!("Length resolves to zero ticks: '{}'", s)));
    }
    Ok(ticks)
}

/// Validate that every track/drum length in a scene resolves to a valid tick count.
/// Used by the config loaders so malformed fractions are rejected up front.
pub fn try_validate_scene_lengths(scene: &SceneConfig) -> Result<(), ReloadError> {
    for t in scene.tracks.values() {
        t.length.to_ticks(scene.ppqn)?;
    }
    if let Some(ref d) = scene.drums {
        if let Some(ref l) = d.length {
            l.to_ticks(scene.ppqn)?;
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Config Loaders
// -----------------------------------------------------------------------------
pub fn load_scene_config(path: &str) -> SceneConfig {
    let config_contents = fs::read_to_string(path)
        .unwrap_or_else(|_| {
            println!("Warning: Scene config file '{}' not found! Falling back to defaults.", path);
            r#"
            bpm = 120
            metronome_on = true
            time_signature_numerator = 4
            time_signature_denominator = 4
            ppqn = 24
            max_tracking_beats = 16
            [tracks.1]
            channel = 0
            length = 96
            program = 0
            "# .to_string()
        });

    let scene: SceneConfig = toml::from_str(&config_contents)
        .expect("Failed to parse scene TOML configuration file");
    try_validate_scene_lengths(&scene).expect("Invalid track/drum length in scene config");
    scene
}

pub fn load_hardware_config(path: &str) -> HardwareConfig {
    let contents = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read hardware config file '{}': {}", path, e));
    toml::from_str(&contents).expect("Failed to parse hardware TOML configuration file")
}

pub fn load_gm_config(path: &str) -> GmConfig {
    let contents = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read GM config file '{}': {}", path, e));
    toml::from_str(&contents).expect("Failed to parse GM TOML configuration file")
}

pub fn gm_name(gm: &GmConfig, program: u8) -> String {
    gm.programs.get(&program.to_string()).cloned().unwrap_or_else(|| format!("Program {}", program))
}

/// Load all *.toml files from a directory as scene configs.
/// Returns a list of (scene_name, config) sorted alphabetically by filename stem.
pub fn load_scenes_from_dir(dir: &str) -> Vec<(String, SceneConfig)> {
    let path = Path::new(dir);
    if !path.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<(String, std::path::PathBuf)> = match fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "toml"))
            .map(|e| {
                let stem = e.path().file_stem().unwrap_or_default().to_string_lossy().to_string();
                (stem, e.path())
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter()
        .map(|(stem, file_path)| {
            let config = load_scene_config(file_path.to_str().unwrap_or(""));
            (stem, config)
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Fallible Config Loaders (for live reload)
// -----------------------------------------------------------------------------

/// Fallible version of load_scene_config for live reload.
pub fn try_load_scene_config(path: &str) -> Result<SceneConfig, ReloadError> {
    let contents = fs::read_to_string(path)?;
    let scene: SceneConfig = toml::from_str(&contents)
        .map_err(|e| ReloadError::Parse(format!("Scene '{}': {}", path, e)))?;
    try_validate_scene_lengths(&scene)
        .map_err(|e| ReloadError::Parse(format!("Scene '{}': {}", path, e)))?;
    Ok(scene)
}

/// Fallible version of load_hardware_config for live reload.
pub fn try_load_hardware_config(path: &str) -> Result<HardwareConfig, ReloadError> {
    let contents = fs::read_to_string(path)?;
    toml::from_str(&contents)
        .map_err(|e| ReloadError::Parse(format!("Hardware '{}': {}", path, e)))
}

/// Fallible version of load_scenes_from_dir for live reload.
pub fn try_load_scenes_from_dir(dir: &str) -> Result<Vec<(String, SceneConfig)>, ReloadError> {
    let path = Path::new(dir);
    if !path.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(String, std::path::PathBuf)> = match fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "toml"))
            .map(|e| {
                let stem = e.path().file_stem().unwrap_or_default().to_string_lossy().to_string();
                (stem, e.path())
            })
            .collect(),
        Err(_) => return Ok(Vec::new()),
    };
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut result = Vec::new();
    for (stem, file_path) in entries {
        let config = try_load_scene_config(file_path.to_str().unwrap_or(""))?;
        result.push((stem, config));
    }
    Ok(result)
}

/// Parse a scene's drum pad mapping (notes per pad + output channel).
/// Returns the defaults (no notes, channel 9) when the scene has no drums section.
pub fn try_scene_drum_mapping(scene: &SceneConfig) -> Result<(Vec<Vec<u8>>, u8), ReloadError> {
    match &scene.drums {
        Some(d) => {
            let notes: Result<Vec<Vec<u8>>, ReloadError> = d.notes.iter().map(|s| {
                s.split('+')
                    .map(|n| n.trim().parse::<u8>()
                        .map_err(|_| ReloadError::Parse(format!("Invalid drum note: '{}'", n))))
                    .collect::<Result<Vec<u8>, _>>()
            }).collect();
            Ok((notes?, d.channel))
        },
        None => Ok((vec![], 9u8)),
    }
}

/// Build KeyMappings from a HardwareConfig + SceneConfig, returning Result for live reload.
pub fn try_build_key_mappings(hw: &HardwareConfig, scene: &SceneConfig) -> Result<KeyMappings, ReloadError> {
    let shift = try_parse_key(&hw.beats_per_measure.modifier)?;
    if shift.channel.is_none() {
        return Err(ReloadError::Parse(
            "beats_per_measure.modifier must include a channel (e.g. CC17_CH1)".to_string()
        ));
    }
    let drum_keys: Vec<_> = match hw.drums.as_ref() {
        Some(d) => d.pads.iter().map(|k| try_parse_key(k)).collect::<Result<Vec<_>, _>>()?,
        None => vec![],
    };
    let (drum_notes, drum_channel) = try_scene_drum_mapping(scene)?;
    let (led_on_channel, led_off_channel, led_blink_channel) = match &hw.leds {
        Some(led) => (
            Some(led.on_channel - 1),
            Some(led.off_channel - 1),
            led.blink_channel.map(|c| c - 1),
        ),
        None => (None, None, None),
    };
    Ok(KeyMappings {
        metronome:    try_parse_key(&hw.metronome.key)?,
        clear_track:  try_parse_key(&hw.clear_track.key)?,
        record_track: try_parse_key(&hw.record_track.key)?,
        clear_all:    try_parse_key(&hw.clear_all.key)?,
        track_keys:   hw.tracks.keys.iter().map(|k| try_parse_key(k)).collect::<Result<Vec<_>, _>>()?,
        shift,
        beat4:        try_parse_key(&hw.beats_per_measure.key4)?,
        beat8:        try_parse_key(&hw.beats_per_measure.key8)?,
        beat16:       try_parse_key(&hw.beats_per_measure.key16)?,
        drum_keys,
        drum_notes,
        drum_channel,
        metronome_led: hw.metronome.led_note,
        record_led:    hw.record_track.led_note,
        led_on_channel,
        led_off_channel,
        led_blink_channel,
        quantize:          hw.quantize.as_ref().map(|q| try_parse_key(&q.key)).transpose()?,
        instrument_select: hw.instruments.as_ref().map(|i| try_parse_key(&i.key)).transpose()?,
        undo_track:        hw.undo_track.as_ref().map(|u| try_parse_key(&u.key)).transpose()?,
        mute_track:        hw.mute_track.as_ref().map(|m| try_parse_key(&m.key)).transpose()?,
        volume_knob:       hw.volume_knob.as_ref().map(|v| try_parse_key(&v.key)).transpose()?,
        bpm_knob:          hw.bpm_knob.as_ref().map(|b| try_parse_key(&b.key)).transpose()?,
        scene_down:        hw.scene_down.as_ref().map(|s| try_parse_key(&s.key)).transpose()?,
        scene_up:          hw.scene_up.as_ref().map(|s| try_parse_key(&s.key)).transpose()?,
    })
}
