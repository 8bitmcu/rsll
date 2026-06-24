mod config;
mod domain;
mod engine;
mod tui;
pub mod export;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};
use std::thread;
use midir::{MidiInput, MidiOutput};

use config::{
    load_gm_config, load_hardware_config, load_scene_config, load_scenes_from_dir,
    parse_key, try_parse_duration, KeyMappings,
    try_load_hardware_config, try_load_scenes_from_dir, try_build_key_mappings,
};
use domain::EngineState;
use engine::{apply_scene_reload, find_port_by_any_regex, find_port_by_regex, handle_midi_message, load_patches, run_master_clock, send_led};
use tui::run_tui;

// -----------------------------------------------------------------------------
// Program Entrypoint
// -----------------------------------------------------------------------------
fn main() {
    // 1. Gather raw CLI arguments
    let args: Vec<String> = std::env::args().collect();
    let mut scene_path: Option<String> = None;
    let mut hw_path = "hardware.toml".to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-s" if i + 1 < args.len() => {
                scene_path = Some(args[i + 1].clone());
                i += 2;
            }
            "-c" if i + 1 < args.len() => {
                hw_path = args[i + 1].clone();
                i += 2;
            }
            "-h" | "--help" => {
                println!("Usage: {} [-s <scene.toml>] [-c <hardware.toml>]", args[0]);
                std::process::exit(0);
            }
            _ => { i += 1; }
        }
    }

    println!("Initializing rsll");
    println!("  Hardware config: {}", hw_path);
    let hw_config = load_hardware_config(&hw_path);
    println!("  Input ports ({}):", hw_config.ports.in_ports.len());
    for (i, p) in hw_config.ports.in_ports.iter().enumerate() {
        if p.exclude_channels.is_empty() {
            println!("    [{}] {} (no channel filter)", i + 1, p.pattern);
        } else {
            println!("    [{}] {} (exclude_channels: {:?})", i + 1, p.pattern, p.exclude_channels);
        }
    }

    // Load scenes: single file if -s given, otherwise all *.toml from scenes/
    let all_scenes: Vec<(String, _)> = if let Some(ref path) = scene_path {
        println!("  Scene config:    {}", path);
        let config = load_scene_config(path);
        let stem = std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "custom".to_string());
        vec![(stem, config)]
    } else {
        let loaded = load_scenes_from_dir("scenes");
        if loaded.is_empty() {
            println!("  Scene config:    scenes/ (no .toml files found, using defaults)");
            vec![("default".to_string(), load_scene_config("config.toml"))]
        } else {
            println!("  Scene config:    scenes/ ({} scenes loaded)", loaded.len());
            for (name, _) in &loaded {
                println!("    - {}", name);
            }
            loaded
        }
    };

    let scenes = Arc::new(RwLock::new(all_scenes));
    let initial_scene_name = scenes.read().unwrap()[0].0.clone();
    let scene_config = scenes.read().unwrap()[0].1.clone();
    let gm_config = load_gm_config("gm.toml");

    let shift = parse_key(&hw_config.beats_per_measure.modifier);
    if shift.channel.is_none() {
        panic!("beats_per_measure.modifier must include a channel (e.g. CC17_CH1)");
    }
    let drum_keys: Vec<_> = hw_config.drums.as_ref()
        .map(|d| d.pads.iter().map(|k| parse_key(k)).collect())
        .unwrap_or_default();
    let (drum_notes, drum_channel) = match &scene_config.drums {
        Some(d) => {
            let notes = d.notes.iter().map(|s| {
                s.split('+')
                    .map(|n| n.trim().parse::<u8>().unwrap_or_else(|_| panic!("Invalid drum note: '{}'", n)))
                    .collect::<Vec<u8>>()
            }).collect();
            (notes, d.channel)
        },
        None => (vec![], 9u8),
    };
    let (led_on_channel, led_off_channel, led_blink_channel) = match &hw_config.leds {
        Some(led) => (
            Some(led.on_channel - 1),
            Some(led.off_channel - 1),
            led.blink_channel.map(|c| c - 1),
        ),
        None => (None, None, None),
    };
    let key_mappings = KeyMappings {
        metronome:    parse_key(&hw_config.metronome.key),
        clear_track:  parse_key(&hw_config.clear_track.key),
        record_track: parse_key(&hw_config.record_track.key),
        clear_all:    parse_key(&hw_config.clear_all.key),
        track_keys:   hw_config.tracks.keys.iter().map(|k| parse_key(k)).collect(),
        shift,
        beat4:        parse_key(&hw_config.beats_per_measure.key4),
        beat8:        parse_key(&hw_config.beats_per_measure.key8),
        beat16:       parse_key(&hw_config.beats_per_measure.key16),
        drum_keys,
        drum_notes,
        drum_channel,
        metronome_led: hw_config.metronome.led_note,
        record_led:    hw_config.record_track.led_note,
        led_on_channel,
        led_off_channel,
        led_blink_channel,
        quantize:          hw_config.quantize.as_ref().map(|q| parse_key(&q.key)),
        instrument_select: hw_config.instruments.as_ref().map(|i| parse_key(&i.key)),
        undo_track:        hw_config.undo_track.as_ref().map(|u| parse_key(&u.key)),
        mute_track:        hw_config.mute_track.as_ref().map(|m| parse_key(&m.key)),
        volume_knob:       hw_config.volume_knob.as_ref().map(|v| parse_key(&v.key)),
        bpm_knob:          hw_config.bpm_knob.as_ref().map(|b| parse_key(&b.key)),
        scene_down:        hw_config.scene_down.as_ref().map(|s| parse_key(&s.key)),
        scene_up:          hw_config.scene_up.as_ref().map(|s| parse_key(&s.key)),
    };
    let key_mappings = Arc::new(RwLock::new(key_mappings));

    // Shared metronome config, record LED, and LED channel for live reload
    let shared_metro = Arc::new(RwLock::new(hw_config.metronome.clone()));
    let shared_record_led: Arc<RwLock<Option<u8>>> = Arc::new(RwLock::new(hw_config.record_track.led_note));
    let shared_led_on_channel: Arc<RwLock<Option<u8>>> = Arc::new(RwLock::new(led_on_channel));

    // --- DYNAMIC TRACK COUNT RESOLUTION ---
    let max_track_id = scene_config.tracks.keys()
        .filter_map(|k| k.parse::<usize>().ok())
        .max()
        .unwrap_or(1);

    println!(">> Dynamic Engine Allocation: Configuring {} total track slots.", max_track_id);

    let mut track_channels = HashMap::new();
    let mut tracks = HashMap::new();
    let mut track_lengths = HashMap::new();
    let mut track_programs = HashMap::new();
    let mut track_volumes = HashMap::new();
    let mut track_velocities = HashMap::new();
    let mut track_velocity_multipliers = HashMap::new();
    let mut track_ids: Vec<usize> = Vec::new();

    for i in 1..=max_track_id {
        let key = i.to_string();
        if let Some(t_cfg) = scene_config.tracks.get(&key) {
            track_channels.insert(i, t_cfg.channel);
            track_lengths.insert(i, t_cfg.length.to_ticks(scene_config.ppqn).unwrap_or(96));
            track_programs.insert(i, t_cfg.program);
            track_volumes.insert(i, t_cfg.volume.unwrap_or(127u8));
            track_velocities.insert(i, t_cfg.velocity);
            track_velocity_multipliers.insert(i, t_cfg.velocity_multiplier.unwrap_or(1.0f32));
        } else {
            track_channels.insert(i, 0);
            track_lengths.insert(i, 96_usize);
            track_programs.insert(i, 0u8);
            track_volumes.insert(i, 127u8);
            track_velocities.insert(i, None);
            track_velocity_multipliers.insert(i, 1.0f32);
        }
        tracks.insert(i, Vec::new());
        track_ids.push(i);
    }

    let mut track_names: HashMap<usize, String> = HashMap::new();
    for i in 1..=max_track_id {
        let key = i.to_string();
        if let Some(t_cfg) = scene_config.tracks.get(&key) {
            if let Some(ref name) = t_cfg.name {
                track_names.insert(i, name.clone());
            }
        }
    }

    let drums_name = scene_config.drums.as_ref().and_then(|d| d.name.clone());


    let ticks_per_measure = scene_config.ppqn * scene_config.time_signature_numerator as usize;

    // Minimum record arm time from [record_track] arm_time (e.g. "3s")
    let record_arm_time = hw_config.record_track.arm_time.as_deref()
        .map(|s| try_parse_duration(s)
            .unwrap_or_else(|e| panic!("Invalid record_track.arm_time: {}", e)))
        .unwrap_or(Duration::ZERO);

    // Track 0 is a dedicated drums looper lane (exists only when drums are configured)
    if let Some(ref drums_cfg) = scene_config.drums {
        track_channels.insert(0, drums_cfg.channel);
        track_lengths.insert(0, drums_cfg.length.as_ref()
            .map(|l| l.to_ticks(scene_config.ppqn).unwrap_or(ticks_per_measure))
            .unwrap_or(ticks_per_measure));
        tracks.insert(0, Vec::new());
    }

    let max_tick = engine::compute_max_tick(
        scene_config.max_tracking_beats * scene_config.ppqn,
        track_lengths.values().copied(),
    );

    let state = Arc::new(Mutex::new(EngineState {
        bpm: scene_config.bpm,
        ppqn: scene_config.ppqn,
        time_signature_numerator: scene_config.time_signature_numerator,
        time_signature_denominator: scene_config.time_signature_denominator,
        ticks_per_measure,
        max_tick,
        current_tick: 0,
        is_recording: false,
        pending_record: false,
        record_armed_at: None,
        record_arm_time,
        metronome_on: scene_config.metronome_on,
        quantize_on: false,
        active_track: 1,
        track_channels,
        tracks,
        track_lengths,
        track_programs,
        track_volumes,
        track_velocities,
        track_velocity_multipliers,
        drum_velocity: scene_config.drums.as_ref()
            .and_then(|d| d.velocity),
        drum_velocity_multiplier: scene_config.drums.as_ref()
            .and_then(|d| d.velocity_multiplier)
            .unwrap_or(1.0),
        track_history: HashMap::new(),
        muted_tracks: HashSet::new(),
        loop_start_lens: HashMap::new(),
        held_notes: HashMap::new(),
        physically_held: HashSet::new(),
        shift_held: false,
        current_scene_idx: 0,
        scene_name: initial_scene_name,
        scene_display_name: scene_config.name.clone(),
        track_ids,
        track_names,
        drums_name,
    }));

    let shared_midi_out = Arc::new(Mutex::new(None));
    let shared_led_out = Arc::new(Mutex::new(None));

    // -----------------------------------------------------------------------------
    // Connection manager thread: Output Client (Target Sound Synth Engine)
    // -----------------------------------------------------------------------------
    let midi_out_clone = shared_midi_out.clone();
    let scene_config_clone = scene_config;
    let out_patterns = hw_config.ports.out.clone();
    let client_name = hw_config.device.client_name.clone();
    thread::spawn(move || {
        loop {
            let has_conn = midi_out_clone.lock().unwrap().is_some();
            if !has_conn {
                if let Ok(midi_out_client) = MidiOutput::new(&format!("{} Output", client_name)) {
                    if let Some((port, _name)) = find_port_by_any_regex(&midi_out_client, &out_patterns) {
                        match midi_out_client.connect(&port, "rsll-router-out-connection") {
                            Ok(mut conn) => {
                                // 200ms sleep to eliminate ALSA initialization race condition bugs
                                thread::sleep(Duration::from_millis(200));
                                load_patches(&mut conn, &scene_config_clone);
                                *midi_out_clone.lock().unwrap() = Some(conn);
                            }
                            Err(_) => {}
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    });

    // -----------------------------------------------------------------------------
    // Connection manager thread: LED Output Client (Controller feedback port)
    // -----------------------------------------------------------------------------
    if let Some(led_cfg) = &hw_config.leds {
        let led_out_clone = shared_led_out.clone();
        let led_patterns = led_cfg.out.clone();
        let client_name = hw_config.device.client_name.clone();
        let state_clone = state.clone();
        let metronome_led_note = hw_config.metronome.led_note;
        let led_on_ch = led_cfg.on_channel - 1;
        let led_off_ch = led_cfg.off_channel - 1;
        thread::spawn(move || {
            loop {
                let has_conn = led_out_clone.lock().unwrap().is_some();
                if !has_conn {
                    if let Ok(midi_out_client) = MidiOutput::new(&format!("{} LED Output", client_name)) {
                        if let Some((port, _name)) = find_port_by_any_regex(&midi_out_client, &led_patterns) {
                            if let Ok(conn) = midi_out_client.connect(&port, "rsll-led-out-connection") {
                                *led_out_clone.lock().unwrap() = Some(conn);
                                // Sync initial LED state so hardware matches engine state on connect
                                if let Some(note) = metronome_led_note {
                                    let metronome_on = state_clone.lock().unwrap().metronome_on;
                                    let ch = if metronome_on { led_on_ch } else { led_off_ch };
                                    send_led(&led_out_clone, note, ch);
                                }
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_secs(2));
            }
        });
    }

    // -----------------------------------------------------------------------------
    // Connection manager threads: one per configured input port pattern
    // -----------------------------------------------------------------------------
    for (idx, port_cfg) in hw_config.ports.in_ports.iter().enumerate() {
        let state_clone = state.clone();
        let midi_out_clone = shared_midi_out.clone();
        let led_out_clone = shared_led_out.clone();
        let scenes_clone = scenes.clone();
        let port_cfg = port_cfg.clone();
        let keys_clone = key_mappings.clone();
        let client_name = hw_config.device.client_name.clone();
        thread::spawn(move || {
            let mut current_conn = None;
            loop {
                if current_conn.is_none() {
                    if let Ok(midi_in_client) = MidiInput::new(&format!("{} Input {}", client_name, idx + 1)) {
                        if let Some((port, _name)) = find_port_by_regex(&midi_in_client, &port_cfg.pattern) {
                            let state_cb = state_clone.clone();
                            let midi_out_cb = midi_out_clone.clone();
                            let led_out_cb = led_out_clone.clone();
                            let keys_cb = keys_clone.clone();
                            let scenes_cb = scenes_clone.clone();
                            let exclude_channels = port_cfg.exclude_channels.clone();
                            if let Ok(conn) = midi_in_client.connect(
                                &port,
                                &format!("rsll-router-in-{}", idx + 1),
                                move |_timestamp, message, _| {
                                    if !message.is_empty() {
                                        let ch_1indexed = (message[0] & 0x0F) + 1;
                                        if exclude_channels.contains(&ch_1indexed) {
                                            return;
                                        }
                                    }
                                    handle_midi_message(message, &state_cb, &midi_out_cb, &led_out_cb, &keys_cb, &scenes_cb);
                                },
                                (),
                            ) {
                                current_conn = Some(conn);
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_secs(2));
            }
        });
    }

    // Fire precision master execution thread loop
    let clock_state = state.clone();
    let clock_out = shared_midi_out.clone();
    let clock_led_out = shared_led_out.clone();
    let clock_metro = shared_metro.clone();
    let clock_record_led = shared_record_led.clone();
    let clock_led_on_ch = shared_led_on_channel.clone();
    thread::spawn(move || {
        run_master_clock(clock_state, clock_out, clock_led_out, clock_metro, clock_record_led, clock_led_on_ch);
    });

    // -------------------------------------------------------------------------
    // Config file watcher thread (poll-based, checks mtime every ~2s)
    // -------------------------------------------------------------------------
    {
        let watcher_state = state.clone();
        let watcher_midi_out = shared_midi_out.clone();
        let watcher_scenes = scenes.clone();
        let watcher_keys = key_mappings.clone();
        let watcher_metro = shared_metro.clone();
        let watcher_record_led = shared_record_led.clone();
        let watcher_led_on_ch = shared_led_on_channel.clone();
        let watcher_hw_path = hw_path.clone();
        let watcher_scene_path = scene_path.clone();
        thread::spawn(move || {
            // Determine which scene source we're watching
            let scenes_dir = if watcher_scene_path.is_none() { Some("scenes".to_string()) } else { None };

            // Get initial mtimes
            let mut last_hw_mtime = get_mtime(&watcher_hw_path);
            let mut last_scene_mtime = if let Some(ref path) = watcher_scene_path {
                get_mtime(path)
            } else if let Some(ref dir) = scenes_dir {
                get_dir_mtime(dir)
            } else {
                None
            };

            loop {
                thread::sleep(Duration::from_secs(2));

                // --- Check hardware.toml ---
                let current_hw_mtime = get_mtime(&watcher_hw_path);
                if current_hw_mtime != last_hw_mtime {
                    last_hw_mtime = current_hw_mtime;
                    eprintln!("[watcher] hardware.toml changed, reloading...");
                    match try_load_hardware_config(&watcher_hw_path) {
                        Ok(new_hw) => {
                            // Get current scene config for rebuilding key mappings
                            let current_scene = {
                                let scenes_guard = watcher_scenes.read().unwrap();
                                let st = watcher_state.lock().unwrap();
                                scenes_guard.get(st.current_scene_idx)
                                    .map(|(_, cfg)| cfg.clone())
                                    .unwrap_or_else(|| scenes_guard[0].1.clone())
                            };
                            match try_build_key_mappings(&new_hw, &current_scene) {
                                Ok(new_keys) => {
                                    *watcher_keys.write().unwrap() = new_keys;
                                    eprintln!("[watcher] key mappings reloaded");
                                }
                                Err(e) => {
                                    eprintln!("[watcher] failed to rebuild key mappings: {}", e);
                                }
                            }
                            // Update metronome hw config
                            *watcher_metro.write().unwrap() = new_hw.metronome.clone();
                            *watcher_record_led.write().unwrap() = new_hw.record_track.led_note;
                            match new_hw.record_track.arm_time.as_deref().map(try_parse_duration).transpose() {
                                Ok(arm_time) => {
                                    watcher_state.lock().unwrap().record_arm_time = arm_time.unwrap_or(Duration::ZERO);
                                }
                                Err(e) => {
                                    eprintln!("[watcher] invalid record_track.arm_time: {}", e);
                                }
                            }
                            let new_led_on = new_hw.leds.as_ref().map(|l| l.on_channel - 1);
                            *watcher_led_on_ch.write().unwrap() = new_led_on;
                            eprintln!("[watcher] hardware config reloaded successfully");
                        }
                        Err(e) => {
                            eprintln!("[watcher] failed to reload hardware.toml: {}", e);
                        }
                    }
                }

                // --- Check scene file(s) ---
                let current_scene_mtime = if let Some(ref path) = watcher_scene_path {
                    get_mtime(path)
                } else if let Some(ref dir) = scenes_dir {
                    get_dir_mtime(dir)
                } else {
                    None
                };

                if current_scene_mtime != last_scene_mtime {
                    last_scene_mtime = current_scene_mtime;
                    eprintln!("[watcher] scene config changed, reloading...");

                    let reload_result = if let Some(ref path) = watcher_scene_path {
                        config::try_load_scene_config(path)
                            .map(|cfg| {
                                let stem = std::path::Path::new(path.as_str())
                                    .file_stem()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "custom".to_string());
                                vec![(stem, cfg)]
                            })
                    } else {
                        try_load_scenes_from_dir("scenes")
                    };

                    match reload_result {
                        Ok(new_scenes) if !new_scenes.is_empty() => {
                            // Get current scene index and name to check if active scene changed
                            let (_current_idx, current_name) = {
                                let st = watcher_state.lock().unwrap();
                                (st.current_scene_idx, st.scene_name.clone())
                            };

                            // Apply reload to active scene if it's still present
                            let active_scene = new_scenes.iter()
                                .find(|(name, _)| *name == current_name)
                                .map(|(_, cfg)| cfg.clone());

                            // Update the shared scenes list
                            *watcher_scenes.write().unwrap() = new_scenes.clone();

                            // Fix up scene index if the active scene moved position
                            if let Some(new_idx) = new_scenes.iter().position(|(name, _)| *name == current_name) {
                                watcher_state.lock().unwrap().current_scene_idx = new_idx;
                            } else {
                                // Active scene was removed or renamed — clamp to 0
                                let mut st = watcher_state.lock().unwrap();
                                st.current_scene_idx = 0;
                                st.scene_name = new_scenes[0].0.clone();
                                st.scene_display_name = new_scenes[0].1.name.clone();
                            }

                            // Apply diff-based reload to the active scene
                            if let Some(scene_cfg) = active_scene {
                                apply_scene_reload(&watcher_state, &watcher_midi_out, &scene_cfg);
                                eprintln!("[watcher] active scene reloaded (diff-based)");
                            }

                            // Rebuild key mappings with new scene's drum config
                            let scene_for_keys = {
                                let scenes_guard = watcher_scenes.read().unwrap();
                                let st = watcher_state.lock().unwrap();
                                scenes_guard.get(st.current_scene_idx)
                                    .map(|(_, cfg)| cfg.clone())
                                    .unwrap_or_else(|| scenes_guard[0].1.clone())
                            };
                            match try_load_hardware_config(&watcher_hw_path) {
                                Ok(hw) => {
                                    if let Ok(new_keys) = try_build_key_mappings(&hw, &scene_for_keys) {
                                        *watcher_keys.write().unwrap() = new_keys;
                                    }
                                }
                                Err(_) => {} // hw config is fine as-is
                            }

                            eprintln!("[watcher] scene config reloaded successfully ({} scenes)", new_scenes.len());
                        }
                        Ok(_) => {
                            eprintln!("[watcher] scene reload produced empty list, keeping current config");
                        }
                        Err(e) => {
                            eprintln!("[watcher] failed to reload scene config: {}", e);
                        }
                    }
                }
            }
        });
    }

    if let Err(e) = run_tui(&state, &gm_config) {
        eprintln!("TUI error: {}", e);
    }
    std::process::exit(0);
}

// -----------------------------------------------------------------------------
// File modification time helpers for the watcher thread
// -----------------------------------------------------------------------------
fn get_mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Get the most recent mtime of any .toml file in a directory.
fn get_dir_mtime(dir: &str) -> Option<SystemTime> {
    let path = std::path::Path::new(dir);
    if !path.is_dir() {
        return None;
    }
    std::fs::read_dir(path).ok()
        .and_then(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map_or(false, |ext| ext == "toml"))
                .filter_map(|e| e.metadata().ok()?.modified().ok())
                .max()
        })
}

#[cfg(test)]
mod tests;
