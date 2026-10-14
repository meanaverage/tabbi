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

On first launch tabbi starts from a copy of your Tabby settings, if you have Tabby installed. Tabby's own settings aren't changed.

## What works

- Local terminals, tabs and split panes
- Tabby's settings, themes and color schemes
- SSH: passwords, key files, the SSH agent, saved passwords (in the Keychain), SFTP and port forwarding
- Moving the window by its tab bar, with the window buttons where Tabby puts them

## Not yet

- Installing plugins, or using plugins installed in Tabby
- More than one window, and Tabby's own app menus
- Telnet and serial connections (left out on purpose)
- Platforms other than macOS (tested on Apple Silicon)

## How it works

Tabby keeps everything that depends on Electron in one plugin, `tabby-electron`. tabbi replaces it with `tabby-tauri` (in `plugin/`), plus a Rust app (`src-tauri/`) for what Electron and Node did: terminals, files, SSH, network connections, the clipboard and the Keychain. `page/` starts Tabby inside the window.

`./build` copies the Tabby checkout into `.build/`, adds these parts to the copy, builds it, then builds the app with Tauri. The Tabby checkout itself is never changed.

`node scripts/test.mjs` checks a built app end to end: terminals, SSH against a throwaway local server, window dragging, settings. Its test window stays behind your other windows and uses its own settings, clipboard and passwords.

## Credits and license

tabbi isn't affiliated with Tabby or its author. Tabby is by [Eugeny](https://github.com/Eugeny) and MIT licensed. The SSH code in `src-tauri/src/ssh` is derived from Eugeny's [russh-napi](https://github.com/Eugeny/russh-napi) (Apache 2.0, see the license file there).

tabbi itself is MIT licensed (see `LICENSE`).
