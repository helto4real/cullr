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
use eframe::egui;
use ffmpeg::{
    ChannelLayout, Rational, codec,
    codec::packet::side_data::Type as PacketSideDataType,
    format, frame, media,
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
const MAX_RGBA_FRAME_BYTES: usize = 512 * 1024 * 1024;

pub struct PlaybackEvent {
    pub playback_id: u64,
    pub path: PathBuf,
    pub frame: Option<egui::ColorImage>,
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

    #[cfg(test)]
    pub(crate) fn test_handle(path: PathBuf, paused: bool) -> Self {
        Self {
            id: NEXT_PLAYBACK_ID.fetch_add(1, Ordering::SeqCst),
            path,
            controls: Arc::new(PlaybackControls {
                stop: AtomicBool::new(false),
                paused: AtomicBool::new(paused),
                muted: AtomicBool::new(true),
            }),
        }
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
    rotation: VideoRotation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoRotation {
    None,
    Clockwise90,
    HalfTurn,
    CounterClockwise90,
}

struct PlaybackTimeline {
    start_at: Duration,
    playback_start: Instant,
    paused_total: Duration,
    first_pts: Option<Duration>,
    fallback_index: u64,
    sent_frames: u64,
    frame_duration: Duration,
    duration: Option<Duration>,
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
        if let Some(frame) = receive_scaled_video_frame(&mut setup)? {
            return Ok(frame.image);
        }
    }

    setup.decoder.send_eof()?;
    if let Some(frame) = receive_scaled_video_frame(&mut setup)? {
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

    let mut timeline = PlaybackTimeline {
        start_at,
        playback_start: Instant::now(),
        paused_total: Duration::ZERO,
        first_pts: None,
        fallback_index: 0,
        sent_frames: 0,
        frame_duration: setup.frame_duration,
        duration,
    };

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
                &mut timeline,
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
            &mut timeline,
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
    timeline: &mut PlaybackTimeline,
) -> bool {
    let fallback_time =
        timeline.start_at + mul_duration(timeline.frame_duration, timeline.fallback_index);
    timeline.fallback_index += 1;
    let pts = frame.timestamp.unwrap_or(fallback_time);
    if should_discard_seek_preroll(frame.timestamp, timeline.start_at) {
        return true;
    }
    let base = *timeline.first_pts.get_or_insert(pts);
    let relative = pts.saturating_sub(base);

    let show_initial_paused_frame =
        timeline.sent_frames == 0 && controls.paused.load(Ordering::SeqCst);
    if !show_initial_paused_frame
        && !wait_until(
            timeline.playback_start,
            relative,
            controls,
            &mut timeline.paused_total,
        )
    {
        return false;
    }
    if timeline.sent_frames > 0 && controls.paused.load(Ordering::SeqCst) {
        return true;
    }

    let size = [frame.image.width() as usize, frame.image.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, frame.image.as_raw());
    let event = PlaybackEvent {
        playback_id,
        path: path.to_path_buf(),
        frame: Some(image),
        position: Some(pts),
        duration: timeline.duration,
        ended: false,
        error: None,
    };
    match frame_tx.try_send(event) {
        Ok(()) => timeline.sent_frames += 1,
        Err(flume::TrySendError::Full(_)) => {}
        Err(flume::TrySendError::Disconnected(_)) => return false,
    }
    true
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
    let rotation = stream_rotation(&stream);
    let decoder_context = codec::context::Context::from_parameters(stream.parameters())?;
    let decoder = decoder_context.decoder().video()?;
    let (width, height) = capped_dimensions_with_aspect(
        decoder.width(),
        decoder.height(),
        cap,
        decoder.aspect_ratio(),
    );
    checked_rgba_buffer_len(width as usize, height as usize)?;
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
        rotation,
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
                image: apply_video_rotation(rgba_image_from_frame(&rgba_frame)?, setup.rotation),
                timestamp,
            }))
        }
        Err(error) if decoder_needs_more_input(error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn stream_rotation(stream: &format::stream::Stream<'_>) -> VideoRotation {
    let Some(side_data) = stream
        .side_data()
        .find(|side_data| side_data.kind() == PacketSideDataType::DisplayMatrix)
    else {
        return VideoRotation::None;
    };
    let data = side_data.data();
    if data.len() < 9 * std::mem::size_of::<i32>() {
        return VideoRotation::None;
    }
    let counter_clockwise_degrees =
        unsafe { ffmpeg::ffi::av_display_rotation_get(data.as_ptr().cast::<i32>()) };
    video_rotation_from_counter_clockwise_degrees(counter_clockwise_degrees)
}

fn video_rotation_from_counter_clockwise_degrees(degrees: f64) -> VideoRotation {
    if !degrees.is_finite() {
        return VideoRotation::None;
    }
    match (degrees.round() as i32).rem_euclid(360) {
        45..=134 => VideoRotation::CounterClockwise90,
        135..=224 => VideoRotation::HalfTurn,
        225..=314 => VideoRotation::Clockwise90,
        _ => VideoRotation::None,
    }
}

fn apply_video_rotation(image: RgbaImage, rotation: VideoRotation) -> RgbaImage {
    match rotation {
        VideoRotation::None => image,
        VideoRotation::Clockwise90 => image::imageops::rotate90(&image),
        VideoRotation::HalfTurn => image::imageops::rotate180(&image),
        VideoRotation::CounterClockwise90 => image::imageops::rotate270(&image),
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
    let buffer_len = checked_rgba_buffer_len(width, height)?;
    let mut pixels = vec![0u8; buffer_len];
    for row in 0..height {
        let src_start = row * stride;
        let dst_start = row * row_len;
        pixels[dst_start..dst_start + row_len]
            .copy_from_slice(&data[src_start..src_start + row_len]);
    }
    RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or_else(|| anyhow!("FFmpeg produced an unexpected RGBA buffer"))
}

fn checked_rgba_buffer_len(width: usize, height: usize) -> Result<usize> {
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= MAX_RGBA_FRAME_BYTES)
        .ok_or_else(|| anyhow!("decoded video frame exceeds the 512 MiB safety limit"))
}

#[cfg(test)]
fn capped_dimensions(width: u32, height: u32, cap: u32) -> (u32, u32) {
    capped_dimensions_with_aspect(width, height, cap, Rational(1, 1))
}

fn capped_dimensions_with_aspect(
    width: u32,
    height: u32,
    cap: u32,
    sample_aspect_ratio: Rational,
) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let Rational(numerator, denominator) = sample_aspect_ratio;
    let pixel_aspect = if numerator > 0 && denominator > 0 {
        numerator as f64 / denominator as f64
    } else {
        1.0
    };
    let display_width = width as f64 * pixel_aspect;
    let display_height = height as f64;
    let scale = if cap == u32::MAX {
        1.0
    } else {
        (cap as f64 / display_width.max(display_height)).min(1.0)
    };
    (
        ((display_width * scale).round() as u32).max(1),
        ((display_height * scale).round() as u32).max(1),
    )
}

fn rational_to_f64(value: Rational) -> Option<f64> {
    let Rational(num, den) = value;
    (num > 0 && den > 0).then_some(num as f64 / den as f64)
}

fn decoder_needs_more_input(error: ffmpeg::Error) -> bool {
    matches!(
        error,
        ffmpeg::Error::Other {
            errno: ffmpeg::error::EAGAIN
        } | ffmpeg::Error::Eof
    )
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
    let output = AudioOutput {
        player: &player,
        controls: &controls,
        channels: dst_channels,
        rate: dst_rate,
        time_base,
        start_at,
    };

    for (stream, packet) in input.packets() {
        if controls.stop.load(Ordering::SeqCst) {
            player.stop();
            return Ok(());
        }
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        receive_and_append_audio(&mut decoder, &mut resampler, &output)?;
    }

    decoder.send_eof()?;
    receive_and_append_audio(&mut decoder, &mut resampler, &output)?;
    while !controls.stop.load(Ordering::SeqCst) && !player.empty() {
        apply_audio_controls(&player, &controls);
        thread::sleep(Duration::from_millis(20));
    }
    player.stop();
    Ok(())
}

struct AudioOutput<'a> {
    player: &'a rodio::Player,
    controls: &'a Arc<PlaybackControls>,
    channels: u16,
    rate: u32,
    time_base: Rational,
    start_at: Duration,
}

fn receive_and_append_audio(
    decoder: &mut codec::decoder::Audio,
    resampler: &mut ResampleContext,
    output: &AudioOutput<'_>,
) -> Result<()> {
    let mut decoded = frame::Audio::empty();
    loop {
        match decoder.receive_frame(&mut decoded) {
            Ok(()) => {}
            Err(error) if decoder_needs_more_input(error) => break,
            Err(error) => return Err(error.into()),
        }
        let timestamp = decoded
            .timestamp()
            .and_then(|value| timestamp_to_duration(value, output.time_base));
        if should_discard_seek_preroll(timestamp, output.start_at) {
            continue;
        }
        let mut audio_frame = frame::Audio::empty();
        resampler.run(&decoded, &mut audio_frame)?;
        append_audio_frame(
            output.player,
            output.controls,
            &audio_frame,
            output.channels,
            output.rate,
        )?;
        while output.player.len() > 32 && !output.controls.stop.load(Ordering::SeqCst) {
            apply_audio_controls(output.player, output.controls);
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
        assert!(checked_rgba_buffer_len(100_000, 100_000).is_err());
    }

    #[test]
    fn applies_sample_aspect_ratio_before_capping() {
        assert_eq!(
            capped_dimensions_with_aspect(720, 576, u32::MAX, Rational(16, 15)),
            (768, 576)
        );
        assert_eq!(
            capped_dimensions_with_aspect(720, 576, 400, Rational(16, 15)),
            (400, 300)
        );
    }

    #[test]
    fn display_matrix_degrees_map_to_image_rotations() {
        assert_eq!(
            video_rotation_from_counter_clockwise_degrees(90.0),
            VideoRotation::CounterClockwise90
        );
        assert_eq!(
            video_rotation_from_counter_clockwise_degrees(-90.0),
            VideoRotation::Clockwise90
        );
        assert_eq!(
            video_rotation_from_counter_clockwise_degrees(180.0),
            VideoRotation::HalfTurn
        );
        let image = RgbaImage::new(4, 2);
        assert_eq!(
            apply_video_rotation(image, VideoRotation::Clockwise90).dimensions(),
            (2, 4)
        );
        let mut clockwise_matrix = [0_i32; 9];
        unsafe {
            ffmpeg::ffi::av_display_rotation_set(clockwise_matrix.as_mut_ptr(), 90.0);
        }
        let detected_degrees =
            unsafe { ffmpeg::ffi::av_display_rotation_get(clockwise_matrix.as_ptr()) };
        assert_eq!(
            video_rotation_from_counter_clockwise_degrees(detected_degrees),
            VideoRotation::Clockwise90
        );
    }

    #[test]
    fn decoder_only_swallows_retry_and_eof_errors() {
        assert!(decoder_needs_more_input(ffmpeg::Error::Other {
            errno: ffmpeg::error::EAGAIN,
        }));
        assert!(decoder_needs_more_input(ffmpeg::Error::Eof));
        assert!(!decoder_needs_more_input(ffmpeg::Error::InvalidData));
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
