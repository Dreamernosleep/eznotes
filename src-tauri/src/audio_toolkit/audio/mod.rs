// Re-export all audio components
mod device;
#[cfg(windows)]
mod loopback;
mod sink;
mod utils;
mod visualizer;

pub use device::{list_output_devices, output_device_name, OutputDeviceInfo};
#[cfg(windows)]
pub use loopback::LoopbackCapture;
pub use sink::{
    AudioFrameCallback, CaptureSink, CaptureVad, CapturedAudio, LevelCallback, VadPolicy,
};
pub use utils::{read_wav_samples, save_wav_file, verify_wav_file};
pub use visualizer::AudioVisualiser;
