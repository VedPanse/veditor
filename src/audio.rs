//! Keyboard audio playback helpers backed by recorded keypress samples.

use crate::*;
use crossterm::event::{Event, KeyCode, KeyEvent};
use rodio::{DeviceSinkBuilder, DeviceSinkError, MixerDeviceSink, Player, buffer::SamplesBuffer};
use std::num::{NonZeroU16, NonZeroU32};

const DEFAULT_KEY_WAV: &[u8] = include_bytes!("../assets/keyboard/default.wav");
const ESCAPE_KEY_WAV: &[u8] = include_bytes!("../assets/keyboard/escape.wav");
const SPACE_KEY_WAV: &[u8] = include_bytes!("../assets/keyboard/space.wav");
const ENTER_KEY_WAV: &[u8] = include_bytes!("../assets/keyboard/enter.wav");
const SILENCE_THRESHOLD: f32 = 0.035;
const TRAILING_PAD_MS: usize = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyboardSound {
    Default,
    Escape,
    Space,
    Enter,
}

pub(crate) trait KeyboardSoundPlayer {
    fn play(&mut self, sound: KeyboardSound);
}

pub(crate) struct KeyboardAudio {
    backend: Option<KeyboardAudioBackend>,
}

struct KeyboardAudioBackend {
    device_sink: MixerDeviceSink,
    default_sound: EmbeddedSample,
    escape_sound: EmbeddedSample,
    space_sound: EmbeddedSample,
    enter_sound: EmbeddedSample,
    active_players: Vec<Player>,
}

#[derive(Clone)]
struct EmbeddedSample {
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
    samples: Vec<f32>,
}

impl KeyboardAudio {
    pub(crate) fn new() -> Self {
        Self {
            backend: KeyboardAudioBackend::new().ok(),
        }
    }
}

impl KeyboardSoundPlayer for KeyboardAudio {
    fn play(&mut self, sound: KeyboardSound) {
        if let Some(backend) = &mut self.backend {
            backend.play(sound);
        }
    }
}

impl KeyboardAudioBackend {
    fn new() -> Result<Self, DeviceSinkError> {
        let mut device_sink = DeviceSinkBuilder::open_default_sink()?;
        device_sink.log_on_drop(false);
        Ok(Self {
            device_sink,
            default_sound: load_embedded_sample(DEFAULT_KEY_WAV),
            escape_sound: load_embedded_sample(ESCAPE_KEY_WAV),
            space_sound: load_embedded_sample(SPACE_KEY_WAV),
            enter_sound: load_embedded_sample(ENTER_KEY_WAV),
            active_players: Vec::new(),
        })
    }

    fn play(&mut self, sound: KeyboardSound) {
        self.active_players.retain(|player| !player.empty());

        let sample = match sound {
            KeyboardSound::Default => &self.default_sound,
            KeyboardSound::Escape => &self.escape_sound,
            KeyboardSound::Space => &self.space_sound,
            KeyboardSound::Enter => &self.enter_sound,
        };

        let player = Player::connect_new(self.device_sink.mixer());
        player.append(SamplesBuffer::new(
            sample.channels,
            sample.sample_rate,
            sample.samples.clone(),
        ));
        self.active_players.push(player);
    }
}

pub(crate) fn keyboard_sound_for_event(event: &Event) -> Option<KeyboardSound> {
    match event {
        Event::Key(key) if is_key_press(key.kind) => keyboard_sound_for_key(*key),
        _ => None,
    }
}

pub(crate) fn play_keyboard_sound_for_event(player: &mut impl KeyboardSoundPlayer, event: &Event) {
    if let Some(sound) = keyboard_sound_for_event(event) {
        player.play(sound);
    }
}

pub(crate) fn keyboard_sound_for_key(key: KeyEvent) -> Option<KeyboardSound> {
    match key.code {
        KeyCode::Modifier(_) | KeyCode::Null => None,
        KeyCode::Esc => Some(KeyboardSound::Escape),
        KeyCode::Enter => Some(KeyboardSound::Enter),
        KeyCode::Char(' ') => Some(KeyboardSound::Space),
        _ => Some(KeyboardSound::Default),
    }
}

fn load_embedded_sample(bytes: &[u8]) -> EmbeddedSample {
    let wav = parse_wav_pcm16(bytes).expect("embedded keyboard wav should be valid PCM16");
    trim_embedded_sample(wav)
}

fn trim_embedded_sample(mut sample: EmbeddedSample) -> EmbeddedSample {
    let channels = usize::from(sample.channels.get());
    let frame_count = sample.samples.len() / channels;
    let trailing_pad_frames = sample.sample_rate.get() as usize * TRAILING_PAD_MS / 1000;

    let first_frame = sample
        .samples
        .chunks_exact(channels)
        .position(|frame| frame.iter().any(|value| value.abs() >= SILENCE_THRESHOLD))
        .unwrap_or(0);
    let last_frame = sample
        .samples
        .chunks_exact(channels)
        .rposition(|frame| frame.iter().any(|value| value.abs() >= SILENCE_THRESHOLD))
        .unwrap_or(frame_count.saturating_sub(1));
    let end_frame = (last_frame + trailing_pad_frames).min(frame_count.saturating_sub(1));

    sample.samples = sample.samples[first_frame * channels..(end_frame + 1) * channels].to_vec();
    apply_fades(&mut sample.samples, channels);
    sample
}

fn apply_fades(samples: &mut [f32], channels: usize) {
    let frame_count = samples.len() / channels;
    if frame_count == 0 {
        return;
    }

    let fade_frames = frame_count.min(32);
    for frame in 0..fade_frames {
        let fade_in = frame as f32 / fade_frames as f32;
        let fade_out = (fade_frames - frame) as f32 / fade_frames as f32;
        for channel in 0..channels {
            samples[frame * channels + channel] *= fade_in;
            let end_index = (frame_count - 1 - frame) * channels + channel;
            samples[end_index] *= fade_out;
        }
    }
}

fn parse_wav_pcm16(bytes: &[u8]) -> Result<EmbeddedSample, &'static str> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("invalid wav header");
    }

    let mut offset = 12;
    let mut channels = None;
    let mut sample_rate = None;
    let mut bits_per_sample = None;
    let mut audio_format = None;
    let mut data = None;

    while offset + 8 <= bytes.len() {
        let chunk_id = &bytes[offset..offset + 4];
        let chunk_size =
            u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        offset += 8;
        if offset + chunk_size > bytes.len() {
            return Err("wav chunk out of bounds");
        }

        match chunk_id {
            b"fmt " => {
                if chunk_size < 16 {
                    return Err("wav fmt chunk too small");
                }
                audio_format = Some(u16::from_le_bytes(
                    bytes[offset..offset + 2].try_into().unwrap(),
                ));
                channels = Some(u16::from_le_bytes(
                    bytes[offset + 2..offset + 4].try_into().unwrap(),
                ));
                sample_rate = Some(u32::from_le_bytes(
                    bytes[offset + 4..offset + 8].try_into().unwrap(),
                ));
                bits_per_sample = Some(u16::from_le_bytes(
                    bytes[offset + 14..offset + 16].try_into().unwrap(),
                ));
            }
            b"data" => {
                data = Some(&bytes[offset..offset + chunk_size]);
            }
            _ => {}
        }

        offset += chunk_size;
        if chunk_size % 2 == 1 {
            offset += 1;
        }
    }

    if audio_format != Some(1) {
        return Err("keyboard wav must be PCM");
    }
    if bits_per_sample != Some(16) {
        return Err("keyboard wav must be 16-bit");
    }

    let channels = NonZeroU16::new(channels.ok_or("wav channel count missing")?)
        .ok_or("wav channel count invalid")?;
    let sample_rate = NonZeroU32::new(sample_rate.ok_or("wav sample rate missing")?)
        .ok_or("wav sample rate invalid")?;
    let data = data.ok_or("wav data chunk missing")?;
    if data.len() % 2 != 0 {
        return Err("wav data chunk misaligned");
    }

    let mut samples = Vec::with_capacity(data.len() / 2);
    for chunk in data.chunks_exact(2) {
        let value = i16::from_le_bytes([chunk[0], chunk[1]]);
        samples.push(value as f32 / i16::MAX as f32);
    }

    Ok(EmbeddedSample {
        channels,
        sample_rate,
        samples,
    })
}
