// macOS capture + encode pipeline.
//
// Path: ScreenCaptureKit (SCStream) -> CVPixelBuffer -> VTCompressionSession (HEVC) -> annex-B NALs.
// Capture and encode share the SCStream sample-handler dispatch queue, so encode is invoked
// synchronously on the capture callback — keeps end-to-end pipeline depth at one frame.

pub mod annex_b;
pub mod capture;
pub mod encode;

pub use capture::Capturer;
pub use encode::{EncodedFrame, Encoder, EncoderConfig};
