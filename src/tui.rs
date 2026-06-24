use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::config::{gm_name, GmConfig};
use crate::domain::EngineState;

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{}{}", n, suffix)
}

/// Render a MIDI volume (0-127) as an 8-cell block bar, e.g. `[████░░░░]`.
fn vol_bar(vol: u8) -> String {
    let filled = ((vol as usize * 8) + 63) / 127;
    let filled = filled.min(8);
    let mut s = String::from("[");
    for i in 0..8 {
        s.push(if i < filled { '█' } else { '░' });
    }
    s.push(']');
    s
}

pub fn draw_ui(
    frame: &mut ratatui::Frame,
    state: &Arc<Mutex<EngineState>>,
    gm_config: &GmConfig,
) {
    let (scene_name, bpm, meter_num, meter_den, ticks_per_measure, is_recording, pending_record, active_track, current_tick, ppqn, active_track_len, track_ids, has_drums, track_event_counts, drum_event_count, track_programs, track_volumes, muted_tracks, track_names, drums_name) = {
        let s = state.lock().unwrap();
        let active_track_len = s.track_lengths.get(&s.active_track).copied().unwrap_or(96);
        let ids = s.track_ids.clone();
        let counts: HashMap<usize, usize> = ids.iter()
            .map(|id| (*id, s.tracks.get(id).map(|v| v.len()).unwrap_or(0)))
            .collect();
        let drum_count = s.tracks.get(&0).map(|v| v.len()).unwrap_or(0);
        let has_drums = s.tracks.contains_key(&0);
        let programs: HashMap<usize, u8> = s.track_programs.clone();
        let volumes: HashMap<usize, u8> = s.track_volumes.clone();
        let muted = s.muted_tracks.clone();
        let track_names = s.track_names.clone();
        let drums_name = s.drums_name.clone();
        let display_name = s.scene_display_name.clone().unwrap_or_else(|| s.scene_name.clone());
        (display_name, s.bpm, s.time_signature_numerator, s.time_signature_denominator, s.ticks_per_measure, s.is_recording, s.pending_record, s.active_track, s.current_tick, s.ppqn, active_track_len, ids, has_drums, counts, drum_count, programs, volumes, muted, track_names, drums_name)
    };

    let active_tick = current_tick % active_track_len;
    let beat = current_tick / ppqn + 1;
    let ticks_per_sixteenth = (ppqn / 4).max(1);
    let step = (current_tick % ppqn) / ticks_per_sixteenth + 1;
    let ratio = if active_track_len > 0 { active_tick as f64 / active_track_len as f64 } else { 0.0 };

    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    // --- Status Bar ---
    let meter = format!("{}/{}", meter_num, meter_den);
    let status_spans: Vec<Span> = if is_recording {
        vec![
            Span::raw(" Scene: "),
            Span::styled(scene_name, Style::default().fg(Color::Cyan)),
            Span::raw(" | BPM: "),
            Span::styled(bpm.to_string(), Style::default().fg(Color::Yellow)),
            Span::raw(" | Meter: "),
            Span::styled(meter, Style::default().fg(Color::Yellow)),
            Span::raw(" | Status:  "),
            Span::styled("● RECORDING", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ]
    } else if pending_record {
        vec![
            Span::raw(" Scene: "),
            Span::styled(scene_name, Style::default().fg(Color::Cyan)),
            Span::raw(" | BPM: "),
            Span::styled(bpm.to_string(), Style::default().fg(Color::Yellow)),
            Span::raw(" | Meter: "),
            Span::styled(meter, Style::default().fg(Color::Yellow)),
            Span::raw(" | Status:  "),
            Span::styled("⊙ ARMED", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]
    } else {
        vec![
            Span::raw(" Scene: "),
            Span::styled(scene_name, Style::default().fg(Color::Cyan)),
            Span::raw(" | BPM: "),
            Span::styled(bpm.to_string(), Style::default().fg(Color::Yellow)),
            Span::raw(" | Meter: "),
            Span::styled(meter, Style::default().fg(Color::Yellow)),
            Span::raw(" | Status: IDLE"),
        ]
    };
    let status_bar = Paragraph::new(Line::from(status_spans))
        .block(Block::default().title("System Control Status").borders(Borders::ALL));
    frame.render_widget(status_bar, chunks[0]);

    // --- Timeline ---
    let timeline_title = format!("Loop Matrix Timeline ({} Beat)", ordinal(beat));

    // Progress bar with measure separators ('|' at each measure boundary)
    let bar_width: usize = (chunks[1].width as usize).saturating_sub(20).max(12);
    let num_measures = if ticks_per_measure > 0 {
        (active_track_len / ticks_per_measure).max(1)
    } else {
        1
    };
    let filled = (ratio.clamp(0.0, 1.0) * bar_width as f64).round() as usize;
    let mut boundaries = std::collections::HashSet::new();
    for m in 1..num_measures {
        boundaries.insert(m * bar_width / num_measures);
    }
    let mut bar = String::from(" [");
    for i in 0..bar_width {
        if boundaries.contains(&i) {
            bar.push('|');
        } else if i < filled {
            bar.push('█');
        } else {
            bar.push('░');
        }
    }
    bar.push_str(&format!("]   {} / {}", active_tick, active_track_len));

    // Beat grid: one box per beat in the measure, filled up to the current beat
    let beat_in_measure = if ppqn > 0 && meter_num > 0 {
        (current_tick / ppqn) % meter_num as usize
    } else {
        0
    };
    let mut grid = String::from("  Beat Grid:  ");
    for i in 0..meter_num as usize {
        if i <= beat_in_measure {
            grid.push_str("[■] ");
        } else {
            grid.push_str("[ ] ");
        }
    }
    grid.push_str(&format!(" (Step: {})", step));

    let timeline = Paragraph::new(vec![
        Line::from(Span::styled(bar, Style::default().fg(Color::Green))),
        Line::from(Span::raw(grid)),
    ])
    .block(Block::default().title(timeline_title).borders(Borders::ALL));
    frame.render_widget(timeline, chunks[1]);

    // --- Track Matrix ---
    let inner_width = (chunks[2].width as usize).saturating_sub(2);
    let header_text = format!(
        " {:<6} {:<32} {:<12} {:<14} {}",
        "ID", "Hardware Routing Node", "Vol", "Event Pool", "Status Buffer"
    );
    let header = Line::from(Span::styled(
        format!("{:<width$}", header_text, width = inner_width),
        Style::default().fg(Color::Black).bg(Color::White),
    ));
    let mut lines = vec![header];

    if has_drums {
        let drum_status = if is_recording {
            "Capturing..."
        } else if drum_event_count > 0 {
            "Sequence Loaded"
        } else {
            "Live / Pass-through"
        };
        let drum_count_str = if drum_event_count > 0 { drum_event_count.to_string() } else { "—".to_string() };
        let drum_label = drums_name.as_deref().unwrap_or("Drums");
        let drum_routing = format!("{} (Ch 10 Global)", drum_label);
        lines.push(Line::from(Span::raw(format!(
            " {:<6} {:<32} {:<12} {:<14} {}",
            "0", drum_routing, "-", drum_count_str, drum_status
        ))));
    }

    for track_id in &track_ids {
        let event_count = track_event_counts.get(track_id).copied().unwrap_or(0);
        let is_muted = muted_tracks.contains(track_id);
        let status_str = if is_muted {
            "Muted"
        } else if *track_id == active_track && is_recording {
            "Capturing..."
        } else if event_count > 0 {
            "Sequence Loaded"
        } else {
            "Empty"
        };
        let is_active = *track_id == active_track;
        let program = track_programs.get(track_id).copied().unwrap_or(0);
        let instrument_name = if let Some(name) = track_names.get(track_id) {
            format!("({}) {}", program, name)
        } else {
            format!("({}) {}", program, gm_name(gm_config, program))
        };
        let vol = track_volumes.get(track_id).copied().unwrap_or(127);
        let line_str = format!(
            " {:<6} {:<32} {:<12} {:<14} {}",
            track_id, instrument_name, vol_bar(vol), event_count, status_str
        );
        let style = if is_muted {
            Style::default().fg(Color::DarkGray)
        } else if is_active {
            Style::default().fg(Color::Magenta)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(line_str, style)));
    }

    let track_matrix = Paragraph::new(lines)
        .block(Block::default().title("Instrumental Arrays").borders(Borders::ALL));
    frame.render_widget(track_matrix, chunks[2]);

    // --- Footer ---
    let footer = Paragraph::new(Line::from(Span::raw(
        " [q] Terminate View Workspace | [s] Save MIDI | [Pads 1-8] Change Active Synth Lane | [CC77] Toggle Record"
    )));
    frame.render_widget(footer, chunks[3]);
}

pub fn run_tui(
    state: &Arc<Mutex<EngineState>>,
    gm_config: &GmConfig,
) -> Result<(), io::Error> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    loop {
        terminal.draw(|frame| {
            draw_ui(frame, state, gm_config);
        })?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.code == KeyCode::Char('q') => break,
                Event::Key(key) if key.code == KeyCode::Char('s') => {
                    let st = state.lock().unwrap();
                    crate::export::export_midi(&st);
                }
                Event::Resize(_, _) => { terminal.clear()?; }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
