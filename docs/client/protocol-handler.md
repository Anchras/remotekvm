# RemoteKVM Client Protocol Handler

Packaged desktop clients should register the `remotekvm` URL scheme so the
browser-based AuthKit flow can return to the native client.

## Auth Callback

The client accepts this callback:

```text
remotekvm://auth?token=<session-jwt>
```

It also accepts `remotekvm:///auth?token=<session-jwt>` for manual testing.
If the callback contains `error=<reason>`, the client surfaces it as a login
failure.

## Runtime Configuration

The loopback callback remains the default because the current server validates
native redirects as loopback URLs. For builds pointed at a server that allows
the protocol callback, set:

```sh
RKVM_AUTH_REDIRECT=deeplink
```

With that setting, the client starts auth with `redirect_uri=remotekvm://auth`.
The protocol URL is also parsed from process arguments, which lets launchers and
manual tests invoke:

```sh
remotekvm-client 'remotekvm://auth?token=test.jwt'
```

## macOS Packaging

Use `packaging/macos/RemoteKVM-client-Info.plist.template` as the app bundle
Info.plist base. It registers `CFBundleURLTypes` for the `remotekvm` scheme.

The current Rust entrypoint handles protocol URLs supplied as launch arguments.
If the bundle runtime delivers URLs to an already-running app via AppKit open-URL
events, wire that platform event into `App::handle_deep_link_url`.

## Windows Packaging

Use `packaging/windows/remotekvm-url-protocol.reg.template` as the registry
template for MSI/WiX or installer scripting. Replace `__CLIENT_EXE_PATH__` with
the installed `remotekvm-client.exe` path, escaped for `.reg` syntax.

The protocol command must pass the URL as the first application argument:

```text
"C:\Program Files\RemoteKVM\remotekvm-client.exe" "%1"
```

The Rust entrypoint removes `remotekvm://...` arguments from the normal CLI
argument stream, applies successful auth callbacks, and surfaces callback
errors in the client UI.
