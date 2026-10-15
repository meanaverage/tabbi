# tabbi

**tabbi is a minimal tabby-tauri:** [Tabby](https://github.com/Eugeny/tabby), the terminal app, running on [Tauri](https://tauri.app) instead of Electron.

It's built from an unmodified Tabby checkout plus the parts in this repo, and the app comes out at about 25 MB.

## Install

On a Mac, with these installed once:

- Apple's command line tools: `xcode-select --install`
- [Node.js](https://nodejs.org) 22 or newer, and Yarn: `npm install -g yarn`
- [Rust](https://rustup.rs)

Then:

```sh
git clone https://github.com/Eugeny/tabby.git
git clone https://github.com/meanaverage/tabbi.git
cd tabbi
./build ../tabby
open out/tabbi.app
```

Move `out/tabbi.app` to Applications to keep it. The first build downloads Tabby's packages and compiles the app, so it takes a while and needs about 10 GB of free disk space. To update, pull both repos and run `./build ../tabby` again.

On first launch, if you have Tabby installed, tabbi asks whether to bring your Tabby settings over. Tabby's own settings aren't changed.

## What works

- Local terminals, tabs and split panes, in as many windows as you like (⌘N)
- Tabby's settings, themes and color schemes
- SSH: passwords, key files, the SSH agent, saved passwords (in the Keychain), SFTP and port forwarding
- Moving the window by its tab bar, with the window buttons where Tabby puts them
- The Mac menu bar, native right-click menus, and your installed fonts in Settings
- The window keeps its size and position, and can dock to a screen edge with a system-wide show/hide hotkey
- Vibrancy, opacity and dark mode from Settings, progress on the Dock icon, and profiles in the Dock icon's menu
- Dropping files on a terminal types their paths

## Not yet

- Installing plugins, or using plugins installed in Tabby
- Telnet and serial connections (left out on purpose)
- Platforms other than macOS (tested on Apple Silicon)

## How it works

Tabby keeps everything that depends on Electron in one plugin, `tabby-electron`. tabbi replaces it with `tabby-tauri` (in `plugin/`), plus a Rust app (`src-tauri/`) for what Electron and Node did: terminals, files, SSH, network connections, the clipboard and the Keychain. `page/` starts Tabby inside the window.

`./build` copies the Tabby checkout into `.build/`, adds these parts to the copy, builds it, then builds the app with Tauri. The Tabby checkout itself is never changed.

`node scripts/test.mjs` checks a built app end to end: terminals, SSH against a throwaway local server, window dragging, settings. Its test window stays behind your other windows and uses its own settings, clipboard and passwords.

`node scripts/bench.mjs` measures the same way how long a flood of terminal output takes to get through, and how quickly typing echoes back.

## Credits and license

tabbi isn't affiliated with Tabby or its author. Tabby is by [Eugeny](https://github.com/Eugeny) and MIT licensed. The SSH code in `src-tauri/src/ssh` is derived from Eugeny's [russh-napi](https://github.com/Eugeny/russh-napi) (Apache 2.0, see the license file there).

tabbi itself is MIT licensed (see `LICENSE`).
