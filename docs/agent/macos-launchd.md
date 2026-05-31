# macOS launchd Agent

RemoteKVM should run as a per-user LaunchAgent on macOS. ScreenCaptureKit and
CGEvent input injection both depend on the logged-in user session and TCC
permissions, so a system LaunchDaemon is the wrong default for the capture path.

## Build

```sh
cargo build -p remotekvm-agent --release --features macos_v0
sudo install -m 0755 target/release/remotekvm-agent /usr/local/bin/remotekvm-agent
```

Without `macos_v0`, the agent still connects to the SaaS server and handles the
control channel, but it will not start ScreenCaptureKit/VideoToolbox.

## Install

1. Copy `packaging/macos/io.remotekvm.agent.plist.template` to
   `~/Library/LaunchAgents/io.remotekvm.agent.plist`.
2. Replace `__SERVER_URL__` with the agent WebSocket URL, for example
   `wss://api.example.com/agent`.
3. Replace `__REGISTRATION_TOKEN__` with the machine registration token.
4. Load it:

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.remotekvm.agent.plist
launchctl enable gui/$(id -u)/io.remotekvm.agent
launchctl kickstart -k gui/$(id -u)/io.remotekvm.agent
```

To unload:

```sh
launchctl bootout gui/$(id -u)/io.remotekvm.agent
```

## Permissions

Run the agent once in the foreground from Terminal before relying on launchd:

```sh
RKVM_REGISTRATION_TOKEN=... /usr/local/bin/remotekvm-agent \
  --server-url wss://api.example.com/agent \
  --require-video
```

Grant the binary or hosting terminal:

- Privacy & Security -> Screen Recording, for ScreenCaptureKit capture.
- Privacy & Security -> Accessibility, for CGEvent input injection.

If `--require-video` is omitted and Screen Recording is missing, the agent logs a
warning and continues control-only. Set `RKVM_REQUIRE_VIDEO=true` or pass
`--require-video` in smoke tests when a missing capture permission should fail
the session loudly.

For production packages, sign and notarize the installed binary before asking
users to grant permissions. TCC grants are tied to the signed code identity, so
replacing an unsigned binary can invalidate a previous permission grant.
