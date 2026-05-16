// Windows-specific agent implementation.
//
// Architecture:
//   - Service Controller: runs in Session 0, manages persistent server connection
//   - Session Agent: runs in active user session, handles capture + encode + WebRTC

pub mod audio;
pub mod capture;
pub mod encode;
pub mod input;
pub mod service;
