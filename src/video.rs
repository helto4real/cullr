use std::{
    num::NonZero,
    path::{Path, PathBuf},
    slice,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as AnyhowContext, Result, anyhow};
use ffmpeg::{
    ChannelLayout, Rational, codec, format, frame, media,
    software::{
        resampling::context::Context as ResampleContext,
        scaling::{context::Context as ScaleContext, flag::Flags as ScaleFlags},
    },
    util::{
        format::{Pixel, sample},
        mathematics::rescale,
    },
};
use ffmpeg_next as ffmpeg;
use image::RgbaImage;
use rodio::buffer::SamplesBuffer;

static NEXT_PLAYBACK_ID: AtomicU64 = AtomicU64::new(1);

pub struct PlaybackEvent {
    pub playback_id: u64,
    pub path: PathBuf,
    pub frame: Option<RgbaImage>,
    pub position: Option<Duration>,
    pub duration: Option<Duration>,
    pub ended: bool,
    pub error: Option<String>,
}

pub struct PlaybackHandle {
    id: u64,
    path: PathBuf,
    controls: Arc<PlaybackControls>,
}

struct PlaybackControls {
    stop: AtomicBool,
    paused: AtomicBool,
    muted: AtomicBool,
}

impl PlaybackHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_paused(&self, paused: bool) {
        self.controls.paused.store(paused, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.controls.paused.load(Ordering::SeqCst)
    }

    pub fn set_muted(&self, muted: bool) {
        self.controls.muted.store(muted, Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.controls.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for PlaybackHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct VideoSetup {
    stream_index: usize,
    time_base: Rational,
    frame_duration: Duration,
    decoder: codec::decoder::Video,
    scaler: ScaleContext,
}

pub fn decode_first_frame_rgba(path: &Path, cap: u32) -> Result<RgbaImage> {
    ffmpeg::init().context("failed to initialize FFmpeg")?;
    let mut input =
        format::input(path).with_context(|| format!("failed to open video {}", path.display()))?;
    let mut setup = open_video_setup(&mut input, cap)?;

    for (stream, packet) in input.packets() {
        if stream.index() != setup.stream_index {
            continue;
        }
        setup.decoder.send_packet(&packet)?;
        while let Some(frame) = receive_scaled_video_frame(&mut setup)? {
            return Ok(frame.image);
        }
    }

    setup.decoder.send_eof()?;
    while let Some(frame) = receive_scaled_video_frame(&mut setup)? {
        return Ok(frame.image);
    }

    Err(anyhow!("no decodable video frames in {}", path.display()))
}

pub fn spawn_playback(
    path: PathBuf,
    cap: u32,
    muted: bool,
    start_at: Duration,
    paused: bool,
    frame_tx: flume::Sender<PlaybackEvent>,
) -> PlaybackHandle {
    let id = NEXT_PLAYBACK_ID.fetch_add(1, Ordering::SeqCst);
    let controls = Arc::new(PlaybackControls {
        stop: AtomicBool::new(false),
        paused: AtomicBool::new(paused),
        muted: AtomicBool::new(muted),
    });
    let thread_controls = controls.clone();
    let thread_path = path.clone();
    thread::spawn(move || {
        if let Err(error) = run_video_playback(
            id,
            thread_path.clone(),
            cap,
            start_at,
            thread_controls,
            frame_tx.clone(),
        ) {
            let _ = frame_tx.send(PlaybackEvent {
                playback_id: id,
                path: thread_path,
                frame: None,
                position: None,
                duration: None,
                ended: true,
                error: Some(format!("{error:#}")),
            });
        }
    });

    PlaybackHandle { id, path, controls }
}

fn run_video_playback(
    playback_id: u64,
    path: PathBuf,
    cap: u32,
    start_at: Duration,
    controls: Arc<PlaybackControls>,
    frame_tx: flume::Sender<PlaybackEvent>,
) -> Result<()> {
    ffmpeg::init().context("failed to initialize FFmpeg")?;

    let mut input =
        format::input(&path).with_context(|| format!("failed to open video {}", path.display()))?;
    let duration = format_duration(input.duration());
    let _ = frame_tx.send(PlaybackEvent {
        playback_id,
        path: path.clone(),
        frame: None,
        position: Some(start_at),
        duration,
        ended: false,
        error: None,
    });
    let mut setup = open_video_setup(&mut input, cap)?;
    seek_input(&mut input, start_at).context("failed to seek video")?;
    setup.decoder.flush();
    spawn_audio_playback(path.clone(), controls.clone(), start_at);

    let playback_start = Instant::now();
    let mut paused_total = Duration::ZERO;
    let mut first_pts = None;
    let mut fallback_index = 0u64;
    let mut sent_frames = 0u64;

    for (stream, packet) in input.packets() {
        if controls.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if stream.index() != setup.stream_index {
            continue;
        }
        setup.decoder.send_packet(&packet)?;
        while let Some(frame) = receive_scaled_video_frame(&mut setup)? {
            if !send_playback_frame(
                playback_id,
                &path,
                frame,
                &controls,
                &frame_tx,
                start_at,
                playback_start,
                &mut paused_total,
                &mut first_pts,
                &mut fallback_index,
                &mut sent_frames,
                setup.frame_duration,
                duration,
            ) {
                return Ok(());
            }
        }
    }

    setup.decoder.send_eof()?;
    while let Some(frame) = receive_scaled_video_frame(&mut setup)? {
        if !send_playback_frame(
            playback_id,
            &path,
            frame,
            &controls,
            &frame_tx,
            start_at,
            playback_start,
            &mut paused_total,
            &mut first_pts,
            &mut fallback_index,
            &mut sent_frames,
            setup.frame_duration,
            duration,
        ) {
            return Ok(());
        }
    }

    controls.stop.store(true, Ordering::SeqCst);
    let _ = frame_tx.send(PlaybackEvent {
        playback_id,
        path,
        frame: None,
        position: duration,
        duration,
        ended: true,
        error: None,
    });
    Ok(())
}

fn send_playback_frame(
    playback_id: u64,
    path: &Path,
    frame: VideoFrame,
    controls: &Arc<PlaybackControls>,
    frame_tx: &flume::Sender<PlaybackEvent>,
    start_at: Duration,
    playback_start: Instant,
    paused_total: &mut Duration,
    first_pts: &mut Option<Duration>,
    fallback_index: &mut u64,
    sent_frames: &mut u64,
    frame_duration: Duration,
    duration: Option<Duration>,
) -> bool {
    let fallback_time = start_at + mul_duration(frame_duration, *fallback_index);
    *fallback_index += 1;
    let pts = frame.timestamp.unwrap_or(fallback_time);
    if should_discard_seek_preroll(frame.timestamp, start_at) {
        return true;
    }
    let base = *first_pts.get_or_insert(pts);
    let relative = pts.saturating_sub(base);

    let show_initial_paused_frame = *sent_frames == 0 && controls.paused.load(Ordering::SeqCst);
    if !show_initial_paused_frame && !wait_until(playback_start, relative, controls, paused_total) {
        return false;
    }

    let sent = frame_tx
        .send(PlaybackEvent {
            playback_id,
            path: path.to_path_buf(),
            frame: Some(frame.image),
            position: Some(pts),
            duration,
            ended: false,
            error: None,
        })
        .is_ok();
    if sent {
        *sent_frames += 1;
    }
    sent
}

fn wait_until(
    playback_start: Instant,
    relative: Duration,
    controls: &Arc<PlaybackControls>,
    paused_total: &mut Duration,
) -> bool {
    loop {
        if controls.stop.load(Ordering::SeqCst) {
            return false;
        }
        wait_while_paused(controls, paused_total);
        let target = playback_start + *paused_total + relative;
        let now = Instant::now();
        if now >= target {
            return true;
        }
        thread::sleep((target - now).min(Duration::from_millis(8)));
    }
}

fn wait_while_paused(controls: &Arc<PlaybackControls>, paused_total: &mut Duration) {
    if !controls.paused.load(Ordering::SeqCst) {
        return;
    }
    let paused_at = Instant::now();
    while controls.paused.load(Ordering::SeqCst) && !controls.stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(20));
    }
    *paused_total += paused_at.elapsed();
}

fn open_video_setup(input: &mut format::context::Input, cap: u32) -> Result<VideoSetup> {
    let stream = input
        .streams()
        .best(media::Type::Video)
        .ok_or(ffmpeg::Error::StreamNotFound)?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let frame_duration = rational_to_f64(stream.avg_frame_rate())
        .filter(|rate| *rate > 0.0)
        .map(|rate| Duration::from_secs_f64(1.0 / rate))
        .unwrap_or_else(|| Duration::from_secs_f64(1.0 / 30.0));
    let decoder_context = codec::context::Context::from_parameters(stream.parameters())?;
    let decoder = decoder_context.decoder().video()?;
    let (width, height) = capped_dimensions(decoder.width(), decoder.height(), cap);
    let scaler = ScaleContext::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        Pixel::RGBA,
        width,
        height,
        ScaleFlags::BILINEAR,
    )?;

    Ok(VideoSetup {
        stream_index,
        time_base,
        frame_duration,
        decoder,
        scaler,
    })
}

struct VideoFrame {
    image: RgbaImage,
    timestamp: Option<Duration>,
}

fn receive_scaled_video_frame(setup: &mut VideoSetup) -> Result<Option<VideoFrame>> {
    let mut decoded = frame::Video::empty();
    match setup.decoder.receive_frame(&mut decoded) {
        Ok(()) => {
            let timestamp = decoded
                .timestamp()
                .and_then(|value| timestamp_to_duration(value, setup.time_base));
            let mut rgba_frame = frame::Video::empty();
            setup.scaler.run(&decoded, &mut rgba_frame)?;
            Ok(Some(VideoFrame {
                image: rgba_image_from_frame(&rgba_frame)?,
                timestamp,
            }))
        }
        Err(_) => Ok(None),
    }
}

fn rgba_image_from_frame(frame: &frame::Video) -> Result<RgbaImage> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.stride(0);
    let row_len = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("video frame row is too wide"))?;
    let data = frame.data(0);
    let mut pixels = vec![0u8; row_len * height];
    for row in 0..height {
        let src_start = row * stride;
        let dst_start = row * row_len;
        pixels[dst_start..dst_start + row_len]
            .copy_from_slice(&data[src_start..src_start + row_len]);
    }
    RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or_else(|| anyhow!("FFmpeg produced an unexpected RGBA buffer"))
}

fn capped_dimensions(width: u32, height: u32, cap: u32) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let long_edge = width.max(height);
    if cap == u32::MAX || long_edge <= cap {
        return (width, height);
    }
    let scale = cap as f64 / long_edge as f64;
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

fn rational_to_f64(value: Rational) -> Option<f64> {
    let Rational(num, den) = value;
    (num > 0 && den > 0).then_some(num as f64 / den as f64)
}

fn timestamp_to_duration(value: i64, time_base: Rational) -> Option<Duration> {
    let seconds = value as f64 * rational_to_f64(time_base)?;
    (seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

fn format_duration(value: i64) -> Option<Duration> {
    timestamp_to_duration(value, rescale::TIME_BASE).filter(|duration| !duration.is_zero())
}

fn duration_to_format_timestamp(duration: Duration) -> i64 {
    duration.as_micros().min(i64::MAX as u128) as i64
}

fn seek_input(input: &mut format::context::Input, start_at: Duration) -> Result<()> {
    if start_at.is_zero() {
        return Ok(());
    }
    let timestamp = duration_to_format_timestamp(start_at);
    input.seek(timestamp, ..timestamp)?;
    Ok(())
}

fn should_discard_seek_preroll(timestamp: Option<Duration>, start_at: Duration) -> bool {
    !start_at.is_zero() && timestamp.is_some_and(|timestamp| timestamp < start_at)
}

fn mul_duration(duration: Duration, count: u64) -> Duration {
    Duration::from_secs_f64(duration.as_secs_f64() * count as f64)
}

fn spawn_audio_playback(path: PathBuf, controls: Arc<PlaybackControls>, start_at: Duration) {
    thread::spawn(move || {
        if let Err(error) = run_audio_playback(&path, controls, start_at) {
            tracing::debug!(path = %path.display(), %error, "audio playback disabled");
        }
    });
}

fn run_audio_playback(
    path: &Path,
    controls: Arc<PlaybackControls>,
    start_at: Duration,
) -> Result<()> {
    let stream_handle = rodio::DeviceSinkBuilder::open_default_sink()
        .context("failed to open default audio output")?;
    let player = rodio::Player::connect_new(stream_handle.mixer());
    apply_audio_controls(&player, &controls);

    ffmpeg::init().context("failed to initialize FFmpeg")?;
    let mut input = format::input(path)
        .with_context(|| format!("failed to open audio from {}", path.display()))?;
    let stream = input
        .streams()
        .best(media::Type::Audio)
        .ok_or(ffmpeg::Error::StreamNotFound)?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let decoder_context = codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = decoder_context.decoder().audio()?;
    seek_input(&mut input, start_at).context("failed to seek audio")?;
    decoder.flush();
    let src_layout = usable_channel_layout(decoder.channel_layout(), decoder.channels());
    let dst_layout = if src_layout.channels() > 2 {
        ChannelLayout::STEREO
    } else {
        src_layout
    };
    let dst_channels = u16::try_from(dst_layout.channels().max(1)).unwrap_or(2);
    let src_rate = decoder.rate().max(1);
    let dst_rate = src_rate;
    let mut resampler = ResampleContext::get(
        decoder.format(),
        src_layout,
        src_rate,
        ffmpeg::format::Sample::F32(sample::Type::Packed),
        dst_layout,
        dst_rate,
    )?;

    for (stream, packet) in input.packets() {
        if controls.stop.load(Ordering::SeqCst) {
            player.stop();
            return Ok(());
        }
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        receive_and_append_audio(
            &mut decoder,
            &mut resampler,
            &player,
            &controls,
            dst_channels,
            dst_rate,
            time_base,
            start_at,
        )?;
    }

    decoder.send_eof()?;
    receive_and_append_audio(
        &mut decoder,
        &mut resampler,
        &player,
        &controls,
        dst_channels,
        dst_rate,
        time_base,
        start_at,
    )?;
    while !controls.stop.load(Ordering::SeqCst) && !player.empty() {
        apply_audio_controls(&player, &controls);
        thread::sleep(Duration::from_millis(20));
    }
    player.stop();
    Ok(())
}

fn receive_and_append_audio(
    decoder: &mut codec::decoder::Audio,
    resampler: &mut ResampleContext,
    player: &rodio::Player,
    controls: &Arc<PlaybackControls>,
    channels: u16,
    rate: u32,
    time_base: Rational,
    start_at: Duration,
) -> Result<()> {
    let mut decoded = frame::Audio::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let timestamp = decoded
            .timestamp()
            .and_then(|value| timestamp_to_duration(value, time_base));
        if should_discard_seek_preroll(timestamp, start_at) {
            continue;
        }
        let mut output = frame::Audio::empty();
        resampler.run(&decoded, &mut output)?;
        append_audio_frame(player, controls, &output, channels, rate)?;
        while player.len() > 32 && !controls.stop.load(Ordering::SeqCst) {
            apply_audio_controls(player, controls);
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

fn append_audio_frame(
    player: &rodio::Player,
    controls: &Arc<PlaybackControls>,
    frame: &frame::Audio,
    channels: u16,
    rate: u32,
) -> Result<()> {
    if frame.samples() == 0 {
        return Ok(());
    }
    apply_audio_controls(player, controls);
    let sample_count = frame
        .samples()
        .checked_mul(channels as usize)
        .ok_or_else(|| anyhow!("audio frame is too large"))?;
    let byte_count = sample_count
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| anyhow!("audio frame is too large"))?;
    let data = frame.data(0);
    if data.len() < byte_count {
        return Ok(());
    }
    let samples = unsafe { slice::from_raw_parts(data.as_ptr() as *const f32, sample_count) };
    let source = SamplesBuffer::new(
        NonZero::new(channels).ok_or_else(|| anyhow!("audio has no channels"))?,
        NonZero::new(rate).ok_or_else(|| anyhow!("audio has no sample rate"))?,
        samples.to_vec(),
    );
    player.append(source);
    Ok(())
}

fn apply_audio_controls(player: &rodio::Player, controls: &Arc<PlaybackControls>) {
    player.set_volume(if controls.muted.load(Ordering::SeqCst) {
        0.0
    } else {
        1.0
    });
    if controls.paused.load(Ordering::SeqCst) {
        player.pause();
    } else {
        player.play();
    }
}

fn usable_channel_layout(layout: ChannelLayout, channels: u16) -> ChannelLayout {
    if !layout.is_empty() {
        layout
    } else if channels > 0 {
        ChannelLayout::default(i32::from(channels))
    } else {
        ChannelLayout::STEREO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_video_dimensions_on_long_edge() {
        assert_eq!(capped_dimensions(4000, 2000, 1000), (1000, 500));
        assert_eq!(capped_dimensions(320, 240, 1000), (320, 240));
    }

    #[test]
    fn discards_seek_preroll_before_requested_target() {
        let target = Duration::from_secs(10);

        assert!(should_discard_seek_preroll(
            Some(Duration::from_secs(9)),
            target
        ));
        assert!(!should_discard_seek_preroll(Some(target), target));
        assert!(!should_discard_seek_preroll(
            Some(Duration::from_secs(11)),
            target
        ));
        assert!(!should_discard_seek_preroll(None, target));
        assert!(!should_discard_seek_preroll(
            Some(Duration::ZERO),
            Duration::ZERO
        ));
    }
}
