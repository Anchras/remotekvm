# Client Architecture

## Overview

The client is a native desktop application that allows users to:
1. Log in via OAuth2
2. View their registered machines
3. Connect to a remote machine with video, audio, and input
4. Manage their account and subscription

## Technology Stack

- **Windowing:** `winit` — Cross-platform window management
- **Rendering:** `wgpu` — Low-level GPU API, cross-platform
- **UI:** `egui` — Immediate-mode GUI, heavily themed for professional look
- **WebRTC:** `webrtc-rs` — P2P media transport
- **Audio:** `cpal` — Cross-platform audio I/O
- **Auth:** OAuth2 via system browser + deep link callback
- **HTTP:** `reqwest` — REST API calls to SaaS server
- **Serialization:** `bincode` — Protocol messages

## Application Structure

```
remotekvm-client
├── main.rs              # Entry point, event loop
├── app.rs               # Application state machine
├── auth.rs              # OAuth2 flow + token storage
├── api.rs               # REST API client
├── websocket.rs         # WebSocket connection to server
├── webrtc.rs            # WebRTC peer connection management
├── video/
│   ├── decoder.rs       # Hardware video decoder abstraction
│   ├── renderer.rs      # wgpu YUV→RGB pipeline
│   └── texture.rs       # GPU texture management
├── audio/
│   ├── decoder.rs       # Opus decode
│   └── playback.rs      # cpal output stream
├── input/
│   ├── capture.rs       # winit → InputEvent
│   └── forward.rs       # InputEvent → DataChannel
└── ui/
    ├── theme.rs         # Custom egui theme (colors, fonts, spacing)
    ├── login.rs         # OAuth2 login screen
    ├── dashboard.rs     # Machine list/grid
    ├── connect.rs       # Connection modal/overlay
    ├── toolbar.rs       # In-session toolbar (fullscreen, disconnect, quality)
    └── settings.rs      # Settings panel
```

## State Machine

```
[Unauthenticated]
    │
    ▼ OAuth2 login
[Authenticating]
    │
    ▼ Token received
[Authenticated]
    │
    ▼ Fetch machines
[Dashboard]
    │
    ├── Click machine ──► [Connecting]
    │                       │
    │                       ▼ WebRTC established
    │                   [Connected]
    │                       │
    │                       ├── Disconnect ──► [Dashboard]
    │                       └── Connection error ──► [Dashboard]
    │
    └── Settings, Logout
```

## UI Design

### Login Screen

```
┌─────────────────────────────┐
│                             │
│        [Logo]               │
│                             │
│    Sign in to RemoteKVM      │
│                             │
│  [Continue with Google]     │
│  [Continue with GitHub]     │
│  [Continue with Microsoft]  │
│                             │
│                             │
└─────────────────────────────┘
```

### Dashboard

```
┌─────────────────────────────────────────┐
│ RemoteKVM    [Search...]    [Profile ▼] │
├─────────────────────────────────────────┤
│                                         │
│  My Machines                            │
│  ┌─────────┐  ┌─────────┐  ┌────────┐ │
│  │ [icon]  │  │ [icon]  │  │ [icon] │ │
│  │ Desktop │  │ Laptop  │  │ Server │ │
│  │ ● Online│  │ ● Online│  │○ Offline│
│  │ [Connect]│  │ [Connect]│  │        │
│  └─────────┘  └─────────┘  └────────┘ │
│                                         │
│  Team: Acme Corp                        │
│  ┌─────────┐  ┌─────────┐             │
│  │ [icon]  │  │ [icon]  │             │
│  │ Workstation│ │ MacBook │             │
│  │ ● Online│  │ ● Online│             │
│  │ [Connect]│  │ [Connect]│             │
│  └─────────┘  └─────────┘             │
│                                         │
└─────────────────────────────────────────┘
```

### Connection View

```
┌─────────────────────────────────────────┐
│ [Disconnect] [Fullscreen] [Quality: HD] │
├─────────────────────────────────────────┤
│                                         │
│                                         │
│      [Video Stream - Full Window]       │
│                                         │
│                                         │
│  [Latency: 12ms] [Bitrate: 15Mbps]      │
└─────────────────────────────────────────┘
```

## Theming

We want a professional, non-developery look. Custom egui theme:

- **Color palette:** Dark mode primary. Slate/grays with accent color (teal or blue)
- **Fonts:** Inter or similar modern sans-serif for UI, monospace for technical readouts
- **Spacing:** Generous padding, rounded corners (8px radius)
- **Animations:** Subtle transitions on hover, connection state changes
- **Machine cards:** Card-based layout with status indicators (colored dots)

Example theme configuration:
```rust
fn setup_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(12.0, 12.0);
    style.spacing.window_margin = egui::Margin::same(20.0);
    style.visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.active.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
    
    style.visuals.panel_fill = egui::Color32::from_rgb(15, 23, 42); // Slate 900
    style.visuals.window_fill = egui::Color32::from_rgb(30, 41, 59); // Slate 800
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(51, 65, 85); // Slate 700
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(6, 182, 212); // Cyan 500
    
    ctx.set_style(style);
}
```

## Video Pipeline

### Hardware Decode

**Windows:** Media Foundation (`IMFTransform` for H.264 decode)
**macOS:** VideoToolbox (`VTDecompressionSession`)

The decoder outputs `NV12` or `P010` frames which are uploaded to wgpu textures.

### Rendering

```
Decoded Frame (NV12)
  → Upload to wgpu Texture (R8 + RG8 dual-plane)
    → YUV→RGB shader (wgpu compute or fragment shader)
      → Render to swap chain
```

**Shader (WGSL):**
```wgsl
@group(0) @binding(0)
var y_texture: texture_2d<f32>;

@group(0) @binding(1)
var uv_texture: texture_2d<f32>;

@group(0) @binding(2)
var sampler: sampler;

@fragment
fn fs_main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let y = textureSample(y_texture, sampler, uv).r;
    let uv_sample = textureSample(uv_texture, sampler, uv).rg - 0.5;
    
    // BT.601/BT.709 conversion
    let r = y + 1.402 * uv_sample.g;
    let g = y - 0.344136 * uv_sample.r - 0.714136 * uv_sample.g;
    let b = y + 1.772 * uv_sample.r;
    
    return vec4<f32>(r, g, b, 1.0);
}
```

### Scaling and Aspect Ratio

- Maintain aspect ratio of the remote desktop
- Letterbox/pillarbox with dark background
- Optional fit-to-screen mode

## Audio Pipeline

```
WebRTC Audio Track (Opus)
  → Opus Decode (48kHz stereo PCM)
    → cpal Output Stream
      → System Audio Output
```

**Latency target:** < 100ms end-to-end

## Input Pipeline

```
winit Event
  → Match event type:
    - CursorMoved → InputEvent::MouseMove { x, y }
    - MouseInput { state: Pressed/Released, button } → InputEvent::MouseButton
    - MouseWheel { delta } → InputEvent::MouseScroll
    - KeyboardInput { state, scancode } → InputEvent::KeyDown/KeyUp
  → Serialize with bincode
    → Send over WebRTC DataChannel
```

**Coordinate mapping:**
- Client window size → Remote desktop resolution
- Normalized coordinates (0.0–1.0) in protocol to handle resolution differences

## WebRTC Integration

### PeerConnection Setup

1. Client receives `ConnectResponse` from server
2. Create PeerConnection with server-provided answer SDP
3. Add transceivers:
   - Video: `recvonly`
   - Audio: `recvonly`
4. Create DataChannel: `control` (for sending InputEvents)
5. Process incoming ICE candidates from server
6. Wait for connection state to reach `connected`

### Connection State Handling

```rust
enum ConnectionState {
    Idle,
    Connecting,      // Signaling in progress
    Connected,         // WebRTC connected, media flowing
    Reconnecting,      // ICE disconnected, attempting recovery
    Disconnected,      // Clean disconnect
    Error(String),     // Fatal error
}
```

## Platform-Specific Notes

### Windows

- **Deep link for OAuth2:** Register `remotekvm://` URL scheme handler
- **Window management:** Native title bar, support for high DPI
- **Video decode:** Media Foundation `IMFTransform`
- **Audio:** WASAPI via cpal

### macOS

- **Deep link for OAuth2:** Register URL scheme in Info.plist
- **Window management:** Native Cocoa title bar, respect dark mode
- **Video decode:** VideoToolbox `VTDecompressionSession`
- **Audio:** Core Audio via cpal
- **Notarization:** Required for distribution (post-MVP)

---

## License
AGPL-3.0-or-later
