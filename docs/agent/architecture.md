# Agent Architecture

## Overview

The agent runs on the host machine and is responsible for:
1. Registering with the SaaS server
2. Capturing the desktop and audio
3. Encoding video and audio
4. Streaming over WebRTC to the client
5. Receiving input events and injecting them locally

## Windows Architecture

### Two-Process Design

Windows services run in Session 0 (non-interactive), but desktop capture requires an active user session. We solve this with a service controller + session agent architecture.

```
┌─────────────────────────────────────────────┐
│          remotekvm-service.exe              │
│          (Windows Service, Session 0)       │
│                                             │
│  - Connects to SaaS server via WebSocket    │
│  - Monitors active user sessions (WTS)      │
│  - Spawns remotekvm-session.exe per session │
│  - Handles machine registration             │
│  - Sends heartbeats                         │
│  - IPC: Named pipes to session agents       │
└────────────────┬────────────────────────────┘
                 │ Named Pipe IPC
                 ▼
┌─────────────────────────────────────────────┐
│        remotekvm-session.exe                │
│        (User Session, e.g., Session 1)      │
│                                             │
│  - DXGI Desktop Duplication                 │
│  - NVENC / Media Foundation encode          │
│  - WASAPI loopback audio capture            │
│  - WebRTC peer connection                   │
│  - SendInput injection                      │
└─────────────────────────────────────────────┘
```

### Service Controller

**Responsibilities:**
- Parse configuration (registry or config file)
- Connect to SaaS server WebSocket with registration token
- Monitor Windows Terminal Services (WTS) for session events
- When a user logs in (session created):
  - Spawn `remotekvm-session.exe` in that session (using `CreateProcessAsUser`)
  - Pass server URL, machine ID, and session token via command line or pipe
- When a user logs out (session ended):
  - Gracefully terminate the session agent
- Send periodic heartbeats to server
- Handle `ConnectRequest` from server by forwarding to the active session agent

**Libraries:**
- `windows-service` crate for service scaffolding
- `wtsapi32` for session monitoring
- `tokio` for async I/O

### Session Agent

**Responsibilities:**
- Connect to service controller via named pipe
- Wait for `ConnectRequest` from service
- When connection requested:
  - Initialize capture pipeline
  - Create WebRTC peer connection
  - Exchange signaling with client (via server relay)
  - Start streaming video and audio
  - Process input events from DataChannel
- Clean up on disconnect

**Pipeline:**

```
DXGI Desktop Duplication
  → D3D11 Texture
    → NVENC Encode (or Media Foundation fallback)
      → H.264 NALs
        → WebRTC Video Track

WASAPI Loopback
  → PCM Audio
    → Opus Encode
      → WebRTC Audio Track

WebRTC DataChannel (control)
  → InputEvent (bincode)
    → SendInput / mouse_event
```

### Capture: DXGI Desktop Duplication

**API:** `IDXGIOutputDuplication` from `dxgi1_2`

**Flow:**
1. Enumerate adapters and outputs
2. Create `IDXGIOutputDuplication` for the primary output
3. In a loop:
   - `AcquireNextFrame(timeout)`
   - Map the desktop texture
   - Convert/copy to NV12 (if needed for encoder)
   - Release frame

**Notes:**
- Requires `Desktop Duplication` API which is available on Windows 8.1+
- Only captures the primary display in MVP; multi-monitor is post-MVP
- Must handle `DXGI_ERROR_ACCESS_LOST` (display mode change, TDR, etc.)

### Encode: NVENC Primary

**API:** NVIDIA Video Codec SDK via raw FFI or `nvenc-rs`

**Why NVENC first:**
- Lowest latency (single-frame encode delay possible)
- Best quality at low bitrates
- Dedicated hardware, doesn't compete with GPU compute
- Industry standard for low-latency streaming

**Configuration:**
- Codec: H.264 (MVP), HEVC (post-MVP for bandwidth savings)
- Profile: High
- Rate control: CBR or VBR with low latency mode
- GOP: Large (we drive keyframes via control messages)
- B-frames: 0 (for minimal latency)

**Fallback:** Media Foundation H.264 encoder
- Works on any Windows 10+ machine
- Uses hardware acceleration when available (Intel Quick Sync, AMD VCE)
- Higher latency than NVENC but acceptable for MVP

### Encode: Media Foundation Fallback

**API:** `IMFTransform` (MFT) for H.264 encoding

**Flow:**
1. Create `IMFTransform` for H.264 encoder
2. Set input type (NV12, target resolution)
3. Set output type (H.264, Annex B or AVC)
4. Process samples in a loop

**Limitations:**
- More overhead than NVENC
- Less control over latency parameters
- But universally available

### Audio: WASAPI Loopback

**API:** `IAudioClient` with `eRender` + `AUDCLNT_STREAMFLAGS_LOOPBACK`

**Flow:**
1. Activate audio client on default render endpoint
2. Initialize in shared mode with loopback flags
3. Capture PCM audio
4. Resample to 48kHz stereo if needed
5. Feed to Opus encoder

**Opus Configuration:**
- Sample rate: 48kHz
- Channels: 2 (stereo)
- Application: `OPUS_APPLICATION_AUDIO` (not VOIP, for music quality)
- Bitrate: 128kbps (adjustable)

### Input Injection

**Mouse:**
- `SendInput` with `INPUT_MOUSE`
- Absolute positioning (normalized 0..65535)
- Button events: `MOUSEEVENTF_LEFTDOWN/UP`, `RIGHTDOWN/UP`, etc.
- Scroll: `MOUSEEVENTF_WHEEL` with `WHEEL_DELTA`

**Keyboard:**
- `SendInput` with `INPUT_KEYBOARD`
- `MapVirtualKey` for HID usage to VK code conversion
- Special keys (Windows key, etc.) handled explicitly

### WebRTC Integration

**PeerConnection:**
- Created on connection request
- Video track: H.264 (from NVENC/MF)
- Audio track: Opus (from WASAPI → Opus)
- DataChannel: `control` (reliable, ordered)
- No ICE servers (Tailscale provides direct connectivity)

**Signaling:**
- Offer/Answer exchanged via server relay (WebSocket)
- ICE candidates also relayed via server
- Once connected, all media flows P2P over Tailscale IPs

---

## macOS Architecture

On macOS, we use a single-process architecture running as a `launchd` background job.

```
┌─────────────────────────────────────────────┐
│        remotekvm-agent (launchd)            │
│                                             │
│  - ScreenCaptureKit capture                 │
│  - VideoToolbox HEVC encode                 │
│  - Core Audio capture                       │
│  - WebRTC peer connection                   │
│  - CGEventPost injection                    │
│  - WebSocket to SaaS server                 │
└─────────────────────────────────────────────┘
```

**Existing code in `apps/host/src/macos/`** can be largely reused, refactored into the agent crate.

---

## Configuration

The agent reads configuration from (in order of precedence):
1. Command-line arguments
2. Environment variables (`RKVM_` prefix)
3. Config file (`%PROGRAMDATA%\RemoteKVM\config.toml` on Windows, `/etc/remotekvm/config.toml` on macOS)

**Example config:**
```toml
[server]
url = "wss://api.remotekvm.io/agent"
registration_token = "rkvm_xxxxxxxxxxxx"

[video]
width = 1920
height = 1080
fps = 60
bitrate_kbps = 20000
encoder = "nvenc" # or "mediafoundation", "videotoolbox"

[audio]
enabled = true
bitrate_kbps = 128

[tailscale]
# Tailscale integration settings
```

---

## Security

1. **Registration token** is a one-time secret used to register the machine. It's shown in the client dashboard when adding a new machine.
2. **Session tokens** are short-lived JWTs issued by the server for each WebRTC session.
3. **WebRTC** uses DTLS-SRTP for end-to-end encryption.
4. **Tailscale** adds WireGuard-level encryption underneath.
5. **Input injection** only occurs during an authenticated, active WebRTC session.
6. **Screen capture** requires OS permissions; the agent must fail gracefully if denied.

---

## License
AGPL-3.0-or-later
