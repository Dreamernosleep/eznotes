//! System-audio capture ("what the other people say") for meeting recording.
//!
//! On Windows, a cpal input stream opened on an *output* device is WASAPI
//! loopback: it delivers whatever the PC plays. The audio is downmixed to mono
//! and resampled to 16 kHz.
//!
//! Given a process id, only that app's audio is captured instead (Windows
//! process loopback, including its child processes): a video or music
//! playing in another app stays out of the meeting.
//!
//! WASAPI loopback delivers nothing while the PC is silent, so the capture
//! fills those gaps with zeros: the output is a continuous 16 kHz timeline that
//! stays aligned with the microphone recording.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use rodio::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rodio::cpal::{self, SampleFormat};
use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler, WindowFunction};

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;

/// Gaps longer than this are filled with silence. Shorter ones are normal
/// delivery jitter.
const GAP_SLACK: Duration = Duration::from_millis(200);
/// How often the capture thread wakes to fill gaps or rebuild a failed stream.
const TICK: Duration = Duration::from_millis(100);
/// Wait before reopening the output device after a stream error (e.g. the
/// default speakers changed).
const REOPEN_DELAY: Duration = Duration::from_secs(1);
const RESAMPLER_CHUNK: usize = 1024;

/// A running loopback capture. Dropping it (or [`Self::stop`]) ends it.
pub struct LoopbackCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackCapture {
    /// Start capturing the default output device, or only process `target`
    /// and its children. `on_audio` receives 16 kHz mono samples, continuous
    /// in time (silence filled), on the capture thread.
    pub fn start(
        target: Option<u32>,
        on_audio: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("meeting-loopback".into())
            .spawn({
                let stop = stop.clone();
                move || run_capture(target, stop, on_audio, ready_tx)
            })?;
        // Fail fast when the first stream can't open; later failures retry.
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                Err(anyhow!("System audio capture did not start in time"))
            }
        }
    }

    /// Stop capturing and wait for the last samples to be delivered.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// An open loopback stream plus the receiving end of its audio.
struct OpenStream {
    // Kept alive for the stream's lifetime; the capture stops on drop.
    _keep: Box<dyn std::any::Any>,
    audio: mpsc::Receiver<Vec<f32>>,
    failed: Arc<AtomicBool>,
    resampler: MonoResampler,
}

fn open_stream(target: Option<u32>) -> Result<OpenStream> {
    if let Some(pid) = target {
        return process::open(pid);
    }
    let host = crate::audio_toolkit::get_cpal_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("No audio output device for system audio capture"))?;
    let supported = device
        .default_output_config()
        .map_err(|e| anyhow!("Cannot read the output device format: {e}"))?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(anyhow!(
            "Unsupported output sample format {:?} for loopback",
            supported.sample_format()
        ));
    }
    let config = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate as usize;

    let (tx, audio) = mpsc::channel::<Vec<f32>>();
    let failed = Arc::new(AtomicBool::new(false));
    let stream = device
        .build_input_stream::<f32, _, _>(
            config,
            move |data: &[f32], _| {
                // Downmix on the audio thread; resampling happens on ours.
                let mono: Vec<f32> = data
                    .chunks_exact(channels)
                    .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                    .collect();
                let _ = tx.send(mono);
            },
            {
                let failed = failed.clone();
                move |e| {
                    // Loopback reports a glitch (xrun) whenever playback
                    // resumes after silence; the stream carries on fine.
                    if e.kind() == cpal::ErrorKind::Xrun {
                        debug!("System audio capture glitch: {e}");
                        return;
                    }
                    warn!("System audio capture error: {e}");
                    failed.store(true, Ordering::Release);
                }
            },
            None,
        )
        .map_err(|e| anyhow!("Cannot open system audio capture: {e}"))?;
    stream
        .play()
        .map_err(|e| anyhow!("Cannot start system audio capture: {e}"))?;
    info!(
        "System audio capture started ({} Hz, {} ch)",
        rate, channels
    );
    Ok(OpenStream {
        _keep: Box::new(stream),
        audio,
        failed,
        resampler: MonoResampler::new(rate, WHISPER_SAMPLE_RATE as usize)?,
    })
}

fn run_capture(
    target: Option<u32>,
    stop: Arc<AtomicBool>,
    mut on_audio: impl FnMut(&[f32]),
    ready: mpsc::Sender<Result<()>>,
) {
    let mut stream = match open_stream(target) {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            Some(s)
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let started = Instant::now();
    let rate = WHISPER_SAMPLE_RATE as u64;
    let slack = GAP_SLACK.as_millis() as u64 * rate / 1000;
    let mut produced: u64 = 0;
    let mut reopen_at: Option<Instant> = None;

    // Pad with silence up to `elapsed - slack`, keeping the timeline aligned.
    let fill_gap = |produced: &mut u64, on_audio: &mut dyn FnMut(&[f32])| {
        let expected = started.elapsed().as_millis() as u64 * rate / 1000;
        let target = expected.saturating_sub(slack);
        if *produced < target {
            let zeros = vec![0.0f32; (target - *produced) as usize];
            on_audio(&zeros);
            *produced = target;
        }
    };

    while !stop.load(Ordering::Acquire) {
        let Some(open) = stream.as_mut() else {
            fill_gap(&mut produced, &mut on_audio);
            if reopen_at.is_some_and(|at| Instant::now() >= at) {
                match open_stream(target) {
                    Ok(s) => {
                        stream = Some(s);
                        reopen_at = None;
                    }
                    Err(e) => {
                        debug!("Reopening system audio capture failed: {e}");
                        reopen_at = Some(Instant::now() + REOPEN_DELAY);
                    }
                }
            }
            thread::sleep(TICK);
            continue;
        };

        if open.failed.load(Ordering::Acquire) {
            warn!("System audio capture lost; reopening");
            stream = None;
            reopen_at = Some(Instant::now() + REOPEN_DELAY);
            continue;
        }

        match open.audio.recv_timeout(TICK) {
            Ok(chunk) => {
                // A silent stretch just ended: pad it before the new audio.
                fill_gap(&mut produced, &mut on_audio);
                let mut emitted = 0u64;
                open.resampler.push(&chunk, |out| {
                    emitted += out.len() as u64;
                    on_audio(out);
                });
                produced += emitted;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => fill_gap(&mut produced, &mut on_audio),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                stream = None;
                reopen_at = Some(Instant::now() + REOPEN_DELAY);
            }
        }
    }

    // Deliver what is still queued so the end of the meeting isn't lost.
    if let Some(open) = stream.as_mut() {
        while let Ok(chunk) = open.audio.try_recv() {
            open.resampler.push(&chunk, &mut on_audio);
        }
    }
}

/// Streaming mono resampler on rubato's FFT resampler (as handy-recorder
/// uses for the microphone).
struct MonoResampler {
    resampler: Option<Fft<f32>>,
    in_buf: Vec<f32>,
    out_buf: Vec<f32>,
}

impl MonoResampler {
    fn new(in_hz: usize, out_hz: usize) -> Result<Self> {
        let resampler = if in_hz == out_hz {
            None
        } else {
            Some(
                Fft::<f32>::new_custom(
                    in_hz,
                    out_hz,
                    RESAMPLER_CHUNK,
                    1,
                    1,
                    WindowFunction::BlackmanHarris2,
                    FixedSync::Input,
                )
                .map_err(|e| anyhow!("Cannot create resampler: {e}"))?,
            )
        };
        let out_len = resampler.as_ref().map_or(0, |r| r.output_frames_max());
        Ok(Self {
            resampler,
            in_buf: Vec::with_capacity(RESAMPLER_CHUNK),
            out_buf: vec![0.0; out_len],
        })
    }

    fn push(&mut self, mut src: &[f32], mut emit: impl FnMut(&[f32])) {
        let Some(resampler) = self.resampler.as_mut() else {
            emit(src);
            return;
        };
        while !src.is_empty() {
            let take = (RESAMPLER_CHUNK - self.in_buf.len()).min(src.len());
            self.in_buf.extend_from_slice(&src[..take]);
            src = &src[take..];
            if self.in_buf.len() < RESAMPLER_CHUNK {
                break;
            }
            let input = SequentialSlice::new(&self.in_buf, 1, RESAMPLER_CHUNK).unwrap();
            let out_frames = self.out_buf.len();
            let mut output = SequentialSlice::new_mut(&mut self.out_buf, 1, out_frames).unwrap();
            let indexing = Indexing::new().partial_len(RESAMPLER_CHUNK);
            match resampler.process_into_buffer(&input, &mut output, Some(&indexing)) {
                Ok((_, written)) => emit(&self.out_buf[..written]),
                Err(e) => warn!("System audio resampling failed: {e}"),
            }
            self.in_buf.clear();
        }
    }
}

/// Windows process loopback: only one app's audio (and its child
/// processes'), delivered by Windows already as 16 kHz mono 16-bit PCM.
mod process {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use log::{info, warn};
    use windows::core::{implement, Interface, Ref};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
        IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        WAVEFORMATEX,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, BLOB, COINIT_MULTITHREADED};
    use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_BLOB;

    use super::{MonoResampler, OpenStream};
    use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;

    /// 100 ns units: 200 ms of buffering.
    const BUFFER_DURATION: i64 = 2_000_000;

    // Windows calls this from its own thread pool, so it must be agile.
    #[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
    struct Activated(mpsc::Sender<()>);

    impl IAgileObject_Impl for Activated_Impl {}

    impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
        fn ActivateCompleted(
            &self,
            _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let _ = self.0.send(());
            Ok(())
        }
    }

    /// The capture thread; stops and joins on drop.
    struct Running {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub(super) fn open(pid: u32) -> Result<OpenStream> {
        let (tx, audio) = mpsc::channel::<Vec<f32>>();
        let failed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread = thread::Builder::new()
            .name("meeting-app-audio".into())
            .spawn({
                let failed = failed.clone();
                let stop = stop.clone();
                move || {
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    }
                    match unsafe { start_client(pid) } {
                        Ok((client, capture, event)) => {
                            let _ = ready_tx.send(Ok(()));
                            if let Err(e) = unsafe { pump(&capture, event, &tx, &stop) } {
                                warn!("App audio capture for process {pid} ended: {e}");
                                failed.store(true, Ordering::Release);
                            }
                            unsafe {
                                let _ = client.Stop();
                                let _ = CloseHandle(event);
                            }
                            // Release the COM objects before COM is torn down.
                            drop(capture);
                            drop(client);
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                    unsafe { CoUninitialize() };
                }
            })?;
        let running = Running {
            stop,
            thread: Some(thread),
        };
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("App audio capture did not start in time"))??;
        info!("Capturing the audio of process {pid} only");
        Ok(OpenStream {
            _keep: Box::new(running),
            audio,
            failed,
            // Windows already converts to 16 kHz: no resampling.
            resampler: MonoResampler::new(
                WHISPER_SAMPLE_RATE as usize,
                WHISPER_SAMPLE_RATE as usize,
            )?,
        })
    }

    unsafe fn start_client(pid: u32) -> Result<(IAudioClient, IAudioCaptureClient, HANDLE)> {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // `PROPVARIANT`'s Drop runs PropVariantClear, which would try to free
        // the blob, but the blob points at `params` on the stack.
        let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &params as *const _ as *mut u8,
                        },
                    },
                }),
            },
        });

        let (done_tx, done_rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(done_tx).into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop),
            &handler,
        )?;
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("App audio activation timed out"))?;
        let mut activated = windows::core::HRESULT(0);
        let mut unknown = None;
        operation.GetActivateResult(&mut activated, &mut unknown)?;
        activated.ok()?;
        let client: IAudioClient = unknown
            .ok_or_else(|| anyhow!("App audio activation returned nothing"))?
            .cast()?;

        let block_align = 2u16;
        let format = WAVEFORMATEX {
            wFormatTag: 1, // PCM
            nChannels: 1,
            nSamplesPerSec: WHISPER_SAMPLE_RATE,
            nAvgBytesPerSec: WHISPER_SAMPLE_RATE * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_DURATION,
            0,
            &format,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok((client, capture, event))
    }

    unsafe fn pump(
        capture: &IAudioCaptureClient,
        event: HANDLE,
        tx: &mpsc::Sender<Vec<f32>>,
        stop: &AtomicBool,
    ) -> Result<()> {
        while !stop.load(Ordering::Acquire) {
            if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let packet = capture.GetNextPacketSize()?;
                if packet == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let samples: Vec<f32> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    vec![0.0; frames as usize]
                } else {
                    std::slice::from_raw_parts(data as *const i16, frames as usize)
                        .iter()
                        .map(|&s| s as f32 / i16::MAX as f32)
                        .collect()
                };
                capture.ReleaseBuffer(frames)?;
                if tx.send(samples).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check against a real app: `EZNOTES_LOOPBACK_PID=<pid> cargo test
    /// process_loopback_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn process_loopback_smoke() {
        let pid: u32 = std::env::var("EZNOTES_LOOPBACK_PID")
            .expect("set EZNOTES_LOOPBACK_PID")
            .parse()
            .unwrap();
        let total = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = total.clone();
        let capture = LoopbackCapture::start(Some(pid), move |s| {
            counter.fetch_add(s.len(), Ordering::Relaxed);
        })
        .expect("capture starts");
        thread::sleep(Duration::from_secs(3));
        capture.stop();
        let got = total.load(Ordering::Relaxed);
        println!("captured {got} samples (~{} s)", got / 16_000);
        assert!(got > 16_000, "expected about 3 s of audio");
    }
}
