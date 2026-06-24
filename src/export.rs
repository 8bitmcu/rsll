use std::time::SystemTime;
use std::fs::File;
use std::io::Write;
use crate::domain::{EngineState, MessageType};
use midly::{Smf, Format, Header, Timing, TrackEvent, TrackEventKind, MidiMessage, MetaMessage};
use midly::num::{u4, u7, u15, u28};

pub fn export_midi(state: &EngineState) {
    let ppqn = state.ppqn as u16;
    let header = Header::new(Format::Parallel, Timing::Metrical(u15::from(ppqn)));
    let mut smf = Smf {
        header,
        tracks: Vec::new(),
    };

    // Track 0 in our EngineState is typically drums (if any), 1.. are synth tracks.
    // We will export all tracks present in `state.tracks`.
    for (_track_id, events) in &state.tracks {
        if events.is_empty() {
            continue;
        }

        let mut sorted_events = events.clone();
        sorted_events.sort_by_key(|e| e.tick);

        let mut track = Vec::new();
        let mut last_tick = 0;

        for event in sorted_events {
            let delta = event.tick.saturating_sub(last_tick) as u32;
            let channel = u4::from(event.channel & 0x0F);
            let data1 = u7::from(event.data1 & 0x7F);
            let data2 = u7::from(event.data2 & 0x7F);

            let message = match event.message_type {
                MessageType::NoteOn => MidiMessage::NoteOn { key: data1, vel: data2 },
                MessageType::NoteOff => MidiMessage::NoteOff { key: data1, vel: data2 },
                MessageType::PitchBend => {
                    // In midly, PitchBend takes a u14 value.
                    let val = ((event.data2 as u16) << 7) | (event.data1 as u16);
                    MidiMessage::PitchBend { bend: midly::PitchBend(midly::num::u14::from(val)) }
                }
            };

            track.push(TrackEvent {
                delta: u28::from(delta),
                kind: TrackEventKind::Midi { channel, message },
            });

            last_tick = event.tick;
        }

        // Add End of Track event
        track.push(TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
        });

        smf.tracks.push(track);
    }

    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    
    let filename = format!("{}_{}.mid", state.scene_name, timestamp);
    
    if let Ok(mut file) = File::create(&filename) {
        let mut buf = Vec::new();
        if smf.write(&mut buf).is_ok() {
            let _ = file.write_all(&buf);
        }
    }
}
