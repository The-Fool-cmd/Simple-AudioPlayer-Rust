use anyhow::{Context, Result, anyhow};
use cpal::{
    Device, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ringbuf::{
    HeapRb,
    traits::{Consumer as RbConsumer, Producer as RbProducer, Split},
};
use std::sync::mpsc::{self, Receiver, Sender};
use std::{
    fs::File,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use symphonia::{
    core::{
        audio::{AudioBufferRef, Signal, SignalSpec},
        codecs::DecoderOptions,
        errors::Error as SymphError,
        formats::{FormatOptions, FormatReader},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
        probe::Hint,
        units::TimeBase,
    },
    default::{get_codecs, get_probe},
};

/// Minimal shared playback state for status/quit/underrun tracking.
#[derive(Debug, Default)]
pub struct SharedState {
    pub quit: AtomicBool,
    pub paused: AtomicBool,
    pub underruns: AtomicU64,
    pub samples_played: AtomicU64,
    pub decoder_done: AtomicBool,
    pub total_samples: AtomicU64,
    // Peak level scaled to 0..1000 for simple UI visualization.
    pub level_peak: AtomicU64,
}

impl SharedState {
    pub fn request_quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

/// Input stream information derived from the file's codec params.
#[derive(Debug, Clone, Copy)]
pub struct InputSpec {
    pub sample_rate: u32,
    pub channels: u16,
    #[allow(dead_code)]
    pub time_base: Option<TimeBase>,
    pub duration_secs: Option<f64>,
}

/// Player ties together decoding, buffering, and audio output.
pub struct Player {
    pub state: Arc<SharedState>,
    _stream: Stream,
    control_tx: Sender<Control>,
}

impl Player {
    /// Create and start a player for a given file path. Returns the Player and an input spec.
    pub fn start(path: &Path) -> Result<(Player, InputSpec)> {
        // Probe input and pick an output config that matches sample rate and 2 channels.
        let input_spec = probe_input(path)?;
        let out_channels = 2;

        let output_spec = make_output_spec(input_spec.sample_rate, out_channels)?;

        // Allocate a small ring buffer to decouple decode and output callbacks.
        let sr = output_spec.config.sample_rate as usize;
        let ch = output_spec.config.channels as usize;
        let buffer_seconds = 0.25_f32;
        let capacity_samples = (sr as f32 * ch as f32 * buffer_seconds) as usize;

        let rb = HeapRb::<f32>::new(capacity_samples);
        let (prod, cons) = rb.split();

        let state = Arc::new(SharedState::default());
        if let Some(dsecs) = input_spec.duration_secs {
            let total =
                (dsecs * (input_spec.sample_rate as f64) * (out_channels as f64)).round() as u64;
            state.total_samples.store(total, Ordering::Relaxed);
        }

        // Start audio output stream.
        let stream = start_stream(&output_spec, cons, state.clone())?;

        // Start decoder thread.
        {
            let path = path.to_owned();
            let state2 = state.clone();
            let (tx, rx) = mpsc::channel::<Control>();
            thread::spawn(move || {
                if let Err(e) = decode_to_ringbuf(&path, state2.clone(), prod, out_channels, rx) {
                    eprintln!("Decoder thread error: {e:?}");
                    state2.request_quit();
                }
            });
            return Ok((
                Player {
                    state,
                    _stream: stream,
                    control_tx: tx,
                },
                input_spec,
            ));
        }
    }
}

/* Output (CPAL) */

struct OutputSpec {
    pub device: Device,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
}

fn default_output_device() -> Result<Device> {
    let host = cpal::default_host();
    host.default_output_device()
        .ok_or_else(|| anyhow!("No default output device found"))
}

fn select_output_config(
    device: &Device,
    desired_rate: u32,
    desired_channels: u16,
) -> Result<(StreamConfig, SampleFormat)> {
    let mut best: Option<(StreamConfig, SampleFormat)> = None;

    let supported = device
        .supported_output_configs()
        .context("Failed to query supported output configs")?;

    for cfg_range in supported {
        let fmt = cfg_range.sample_format();

        if desired_rate >= cfg_range.min_sample_rate()
            && desired_rate <= cfg_range.max_sample_rate()
        {
            let cfg = cfg_range.with_sample_rate(desired_rate).config();
            if cfg.channels == desired_channels {
                best = Some((cfg, fmt));
                break;
            }
            // Fallback: rate matches, but channels differ.
            best.get_or_insert((cfg, fmt));
        }
    }

    best.ok_or_else(|| {
        anyhow!(
            "No supported output config matches sample rate {} Hz (and ideally {} channels).",
            desired_rate,
            desired_channels
        )
    })
}

fn start_stream<P>(spec: &OutputSpec, cons: P, state: Arc<SharedState>) -> Result<Stream>
where
    P: RbConsumer<Item = f32> + Send + 'static,
{
    let cfg = spec.config.clone();
    let channels = cfg.channels as usize;
    let mut cons = cons;

    let err_fn = |err| eprintln!("CPAL stream error: {err}");

    let stream = match spec.sample_format {
        SampleFormat::F32 => spec.device.build_output_stream(
            &cfg,
            move |out: &mut [f32], _| {
                fill_output(out, channels, &mut cons, &state);
            },
            err_fn,
            None,
        )?,
        SampleFormat::I16 => spec.device.build_output_stream(
            &cfg,
            move |out: &mut [i16], _| {
                fill_output(out, channels, &mut cons, &state);
            },
            err_fn,
            None,
        )?,
        SampleFormat::U16 => spec.device.build_output_stream(
            &cfg,
            move |out: &mut [u16], _| {
                fill_output(out, channels, &mut cons, &state);
            },
            err_fn,
            None,
        )?,
        SampleFormat::U8 => spec.device.build_output_stream(
            &cfg,
            move |out: &mut [u8], _| {
                fill_output_u8(out, channels, &mut cons, &state);
            },
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("Unsupported sample format from device: {other:?}")),
    };

    stream.play()?;
    Ok(stream)
}

fn make_output_spec(desired_rate: u32, desired_channels: u16) -> Result<OutputSpec> {
    let device = default_output_device()?;
    let (config, sample_format) = select_output_config(&device, desired_rate, desired_channels)?;
    if config.sample_rate != desired_rate {
        return Err(anyhow!(
            "Sample rate mismatch: input {} Hz, output {} Hz (no resampling)",
            desired_rate,
            config.sample_rate
        ));
    }
    Ok(OutputSpec {
        device,
        config,
        sample_format,
    })
}

/// Convert f32 [-1, 1] to device sample type and write it out.
/// Also tracks underruns and played samples.
fn fill_output<T, P>(out: &mut [T], _channels: usize, cons: &mut P, state: &SharedState)
where
    T: cpal::Sample + FromF32,
    P: RbConsumer<Item = f32>,
{
    use std::sync::atomic::Ordering;

    let mut had_underrun = false;
    let mut peak: f32 = 0.0;

    for sample in out.iter_mut() {
        if state.paused.load(Ordering::Relaxed) {
            *sample = f32_to_sample::<T>(0.0);
            continue;
        }

        if let Some(x) = cons.try_pop() {
            // track peak (absolute)
            let abs = x.abs();
            if abs > peak {
                peak = abs;
            }
            *sample = f32_to_sample::<T>(x);
            state.samples_played.fetch_add(1, Ordering::Relaxed);
        } else {
            *sample = f32_to_sample::<T>(0.0);
            had_underrun = true;
            state.samples_played.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Update peak level scaled to 0..1000 range for the UI visualizer.
    // Use relaxed ordering since it's polled by the UI thread.
    let scaled = (peak.clamp(0.0, 1.0) * 1000.0).round() as u64;
    state.level_peak.store(scaled, Ordering::Relaxed);

    if had_underrun {
        state.underruns.fetch_add(1, Ordering::Relaxed);
    }
}

fn fill_output_u8<P>(out: &mut [u8], _channels: usize, cons: &mut P, state: &SharedState)
where
    P: RbConsumer<Item = f32>,
{
    use std::sync::atomic::Ordering;

    let mut had_underrun = false;
    let mut peak: f32 = 0.0;

    for sample in out.iter_mut() {
        if state.paused.load(Ordering::Relaxed) {
            *sample = f32_to_u8(0.0);
            continue;
        }

        if let Some(x) = cons.try_pop() {
            let abs = x.abs();
            if abs > peak {
                peak = abs;
            }
            *sample = f32_to_u8(x);
            state.samples_played.fetch_add(1, Ordering::Relaxed);
        } else {
            *sample = f32_to_u8(0.0);
            had_underrun = true;
            state.samples_played.fetch_add(1, Ordering::Relaxed);
        }
    }

    let scaled = (peak.clamp(0.0, 1.0) * 1000.0).round() as u64;
    state.level_peak.store(scaled, Ordering::Relaxed);

    if had_underrun {
        state.underruns.fetch_add(1, Ordering::Relaxed);
    }
}

/* Decode (Symphonia) */

fn open_format(path: &Path) -> Result<Box<dyn FormatReader>> {
    let file = File::open(path).with_context(|| format!("Failed to open {:?}", path))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("Probe/format error: {e}"))?;

    Ok(probed.format)
}

fn probe_input(path: &Path) -> Result<InputSpec> {
    let format = open_format(path)?;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No default audio track found"))?;

    let params = &track.codec_params;

    let sample_rate = params
        .sample_rate
        .ok_or_else(|| anyhow!("Missing sample_rate in codec params"))?;

    let channels = params
        .channels
        .ok_or_else(|| anyhow!("Missing channels in codec params"))?
        .count() as u16;

    Ok(InputSpec {
        sample_rate,
        channels,
        time_base: track.codec_params.time_base,
        duration_secs: stream_duration_secs(&format, track.codec_params.time_base, sample_rate),
    })
}

fn decode_to_ringbuf<P>(
    path: &Path,
    state: Arc<SharedState>,
    mut prod: P,
    out_channels: u16,
    control_rx: Receiver<Control>,
) -> Result<InputSpec>
where
    P: RbProducer<Item = f32>,
{
    let mut format = open_format(path)?;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No default audio track found"))?;

    let track_id = track.id;
    let params = track.codec_params.clone();

    let input = {
        let sample_rate = params
            .sample_rate
            .ok_or_else(|| anyhow!("Missing sample_rate in codec params"))?;
        let channels = params
            .channels
            .ok_or_else(|| anyhow!("Missing channels in codec params"))?
            .count() as u16;
        InputSpec {
            sample_rate,
            channels,
            time_base: params.time_base,
            duration_secs: None,
        }
    };

    if input.channels > 2 {
        return Err(anyhow!(
            "Only mono/stereo input is supported, got {} channels",
            input.channels
        ));
    }

    let mut decoder = get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| anyhow!("Decoder init error: {e}"))?;

    let mut scratch: Vec<f32> = Vec::new();

    loop {
        if state.quit.load(Ordering::Relaxed) {
            break;
        }
        if state.paused.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        if let Ok(ctrl) = control_rx.try_recv() {
            match ctrl {
                Control::SeekBy(delta_secs) => {
                    // compute target secs from current samples
                    let cur_samples = state.samples_played.load(Ordering::Relaxed);
                    let cur_secs = samples_to_seconds(cur_samples, input.sample_rate, out_channels);
                    let target_secs = (cur_secs + delta_secs).max(0.0);
                    if input.time_base.is_some() {
                        let time = symphonia::core::units::Time {
                            seconds: target_secs as u64,
                            frac: 0.0,
                        };
                        let seek_to = symphonia::core::formats::SeekTo::Time {
                            time,
                            track_id: Some(track_id),
                        };
                        let _ = format.seek(symphonia::core::formats::SeekMode::Accurate, seek_to);
                        // reset samples_played so UI reflects new position accurately
                        let new_samples =
                            (target_secs * (input.sample_rate as f64) * (out_channels as f64))
                                .round() as u64;
                        state.samples_played.store(new_samples, Ordering::Relaxed);
                    }
                    // drain producer to avoid mixing old data after seek
                    let tmp = Vec::new();
                    while prod.push_slice(&tmp) > 0 {}
                }
            }
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphError::ResetRequired) => break,
            Err(SymphError::IoError(_)) => break,
            Err(e) => return Err(anyhow!("Packet error: {e}")),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphError::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("Decode error: {e}")),
        };

        scratch.clear();
        push_as_f32_interleaved(&decoded, &mut scratch)?;

        let adapted: Vec<f32>;
        let samples: &[f32] = if input.channels == 1 && out_channels == 2 {
            adapted = mono_to_stereo(&scratch);
            &adapted
        } else if input.channels == out_channels {
            &scratch
        } else {
            return Err(anyhow!(
                "Channel mismatch: input {}ch, output {}ch",
                input.channels,
                out_channels
            ));
        };

        push_slice_backpressure(&mut prod, samples, &state);
    }
    state.decoder_done.store(true, Ordering::Relaxed);
    // Stop playback when decoder finishes.
    state.request_quit();

    Ok(input)
}

fn push_slice_backpressure<P>(prod: &mut P, mut data: &[f32], state: &SharedState)
where
    P: RbProducer<Item = f32>,
{
    while !data.is_empty() && !state.quit.load(Ordering::Relaxed) {
        let written = prod.push_slice(data);
        data = &data[written..];
        if !data.is_empty() {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

fn mono_to_stereo(mono_interleaved: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(mono_interleaved.len() * 2);
    for &x in mono_interleaved {
        out.push(x);
        out.push(x);
    }
    out
}

fn push_as_f32_interleaved(decoded: &AudioBufferRef<'_>, out: &mut Vec<f32>) -> Result<()> {
    match decoded {
        AudioBufferRef::F32(buf) => {
            let chans = buf.spec().channels.count();
            let frames = buf.frames();
            out.reserve(frames * chans);
            for f in 0..frames {
                for c in 0..chans {
                    out.push(buf.chan(c)[f]);
                }
            }
        }
        AudioBufferRef::U8(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| {
                // u8 PCM is typically unsigned; map [0,255] to [-1,1]
                let x = buf.chan(c)[f] as f32;
                (x / 127.5) - 1.0
            },
            out,
        ),
        AudioBufferRef::U16(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| {
                let x = buf.chan(c)[f] as f32;
                (x / 32767.5) - 1.0
            },
            out,
        ),
        AudioBufferRef::U24(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| {
                let x = buf.chan(c)[f].inner() as f32;
                (x / 8_388_607.5) - 1.0
            },
            out,
        ),
        AudioBufferRef::U32(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| {
                let x = buf.chan(c)[f] as f32;
                (x / 2_147_483_647.5) - 1.0
            },
            out,
        ),
        AudioBufferRef::S8(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| (buf.chan(c)[f] as f32) / 127.0,
            out,
        ),
        AudioBufferRef::S16(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| (buf.chan(c)[f] as f32) / 32768.0,
            out,
        ),
        AudioBufferRef::S24(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| (buf.chan(c)[f].inner() as f32) / 8_388_608.0,
            out,
        ),
        AudioBufferRef::S32(buf) => interleave_to_f32(
            *buf.spec(),
            buf.frames(),
            |c, f| (buf.chan(c)[f] as f32) / 2_147_483_648.0,
            out,
        ),
        AudioBufferRef::F64(buf) => {
            interleave_to_f32(*buf.spec(), buf.frames(), |c, f| buf.chan(c)[f] as f32, out)
        }
    }
    Ok(())
}

fn interleave_to_f32<F>(spec: SignalSpec, frames: usize, mut get: F, out: &mut Vec<f32>)
where
    F: FnMut(usize, usize) -> f32,
{
    let chans = spec.channels.count();
    out.reserve(frames * chans);
    for f in 0..frames {
        for c in 0..chans {
            out.push(get(c, f));
        }
    }
}

/* Utility helpers */

fn clamp_f32(x: f32) -> f32 {
    x.max(-1.0).min(1.0)
}

pub trait FromF32 {
    fn from_f32(x: f32) -> Self;
}

fn f32_to_sample<T: cpal::Sample + FromF32>(x: f32) -> T {
    T::from_f32(clamp_f32(x))
}

impl FromF32 for f32 {
    #[inline]
    fn from_f32(x: f32) -> Self {
        clamp_f32(x)
    }
}

impl FromF32 for i16 {
    #[inline]
    fn from_f32(x: f32) -> Self {
        let scaled = (x * 32768.0).round();
        scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
    }
}

impl FromF32 for u16 {
    #[inline]
    fn from_f32(x: f32) -> Self {
        let shifted = (x + 1.0) * 0.5;
        let scaled = (shifted * 65535.0).round();
        scaled.clamp(u16::MIN as f32, u16::MAX as f32) as u16
    }
}

fn f32_to_u8(x: f32) -> u8 {
    let x = clamp_f32(x);
    let shifted = (x + 1.0) * 0.5; // [0.0, 1.0]
    let scaled = (shifted * 255.0).round();
    scaled.clamp(u8::MIN as f32, u8::MAX as f32) as u8
}

fn samples_to_seconds(samples: u64, sample_rate: u32, channels: u16) -> f64 {
    let denom = (sample_rate as f64) * (channels as f64);
    (samples as f64) / denom
}

fn stream_duration_secs(
    format: &Box<dyn FormatReader>,
    time_base: Option<TimeBase>,
    sample_rate: u32,
) -> Option<f64> {
    // Try duration from timebase if available.
    if time_base.is_some() {
        if let Some(frames) = format
            .tracks()
            .iter()
            .next()
            .and_then(|t| t.codec_params.n_frames)
        {
            let secs = (frames as f64) / (sample_rate as f64);
            return Some(secs);
        }
        // Without n_frames, duration may require reading metadata; return None.
    }
    None
}

#[derive(Debug)]
enum Control {
    SeekBy(f64),
}

impl Player {
    pub fn seek_seconds(&self, delta_secs: f64) {
        let _ = self.control_tx.send(Control::SeekBy(delta_secs));
    }
}
