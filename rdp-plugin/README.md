# Bundled Remote Desktop

This is the Tauri adaptation of [tabby-rdp](https://github.com/meanaverage/tabby-rdp), based on version **0.5.6**, source commit `f27955e`. The original MIT license and IronRDP vendor licenses are included. The vendor JavaScript and WASM are unchanged from that release.

The connection, shared-drive, process, and clipboard bridges started from the Tauri implementation in `meanaverage/tabbz` at `fa3ae85`. They have been adapted to this host and the current plugin's APIs. Native transport lives in `src-tauri/src/rdp/`; the page runs IronRDP and the plugin interface. No Node or Electron installation is required at runtime.

Create a **Remote desktop (RDP)** profile in Settings → Profiles & connections. It can connect directly, through an SSH profile, or through an RD Gateway. Settings → Remote Desktop contains the plugin's preferences, accounts, shared folders, and help. An SSH terminal also gains the desktop controls.

## Host-specific changes

- Built and loaded as a built-in plugin, with its scripts and WASM served from the app's own assets.
- Native loopback WebSocket/TLS proxy, authenticated both at the HTTP upgrade and in the RDCleanPath request. Direct and gateway certificate trust uses the system certificate store; unverified certificates require confirmation. Non-NLA approval remains bound to the accepted certificate.
- SSH desktop channels stay native, independently of terminal output. Closing/reloading a page releases owned proxies, files, temporary downloads, and subprocesses.
- Current release's sign-in/account revisions, clipboard direction controls, bounded streaming download/upload logic, and separate WASM instances are retained. Shared drives use the native confined filesystem implementation.
- Browser clipboard methods use the native clipboard bridge. Test copies use a private clipboard and test password store.
- Plugin updates are delivered with app builds. The npm installer is not offered here because that package requires Electron.
- Certificate prompts retain the fingerprint and native validation verdict; the Node-specific detailed X.509 subject/issuer display is not supplied by the native bridge.

## Updating

Update this source from a specific tabby-rdp release, preserve these host adaptations, and update this provenance note. Do not replace the directory with the npm bundle. Run `node scripts/check-rdp-assets.mjs` to verify the vendor files against `vendor/SHA256SUMS`, build both hosts, run the native tests and shared `scripts/rdp-checks.mjs` checks, and test an actual desktop before a release intended for broad distribution.

iTabby and tabbi build independently. Neither consumes the other's working tree. Shared RDP changes must be brought into both repositories explicitly.
