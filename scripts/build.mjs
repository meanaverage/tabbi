#!/usr/bin/env node
// tabbi's one command: `./build <path to a Tabby checkout>` (add `--debug` for a quicker, unoptimized app).
//
// 1. Copies Tabby's files into .build/tabby. The Tabby checkout itself is never changed.
// 2. Adds this project's two parts to that copy, next to Tabby's own plugins: plugin/ (the Tauri platform plugin,
//    standing in for tabby-electron) as tabby-tauri, and page/ (what starts Tabby in the window) as tauri-page.
// 3. Installs the copy's packages and builds Tabby's plugins, ours and the page into .build/page.
// 4. Builds the app with Tauri (src-tauri/) into out/tabbi.app.
import * as fs from 'node:fs'
import * as path from 'node:path'
import * as url from 'node:url'
import { createRequire } from 'node:module'
import { execFileSync } from 'node:child_process'
import { promisify } from 'node:util'

const root = path.resolve(url.fileURLToPath(new URL('.', import.meta.url)), '..')
const work = path.join(root, '.build/tabby')
const page = path.join(root, '.build/page')

// Tabby's plugins the app loads, in load order, then ours.
const TABBY_PLUGINS = [
    'tabby-core',
    'tabby-settings',
    'tabby-terminal',
    'tabby-community-color-schemes',
    'tabby-ssh',
    'tabby-local',
    'tabby-linkifier',
]
const PLUGINS = [...TABBY_PLUGINS, 'tabby-tauri', 'tabby-rdp']

const args = process.argv.slice(2)
const debug = args.includes('--debug')
const tabbyArg = args.find(a => !a.startsWith('--'))
if (!tabbyArg) {
    console.error('usage: ./build <path to a Tabby checkout> [--debug]')
    process.exit(2)
}
const tabby = path.resolve(tabbyArg)
if (!fs.existsSync(path.join(tabby, 'tabby-core/package.json')) || !fs.existsSync(path.join(tabby, 'app/src/app.module.ts'))) {
    console.error(`${tabby} doesn't look like a Tabby checkout (github.com/Eugeny/tabby)`)
    process.exit(2)
}

const step = (message) => console.log(`\n== ${message}`)

function run (command, commandArgs, cwd, env = {}) {
    execFileSync(command, commandArgs, { cwd, stdio: 'inherit', env: { ...process.env, ...env } })
}

function need (command, hint) {
    try {
        execFileSync(command, ['--version'], { stdio: 'ignore' })
    } catch {
        console.error(`${command} is needed: ${hint}`)
        process.exit(2)
    }
}

need('yarn', 'install Node.js 22 or newer, then run `npm install -g yarn`')
need('rustup', 'install Rust from https://rustup.rs')
need('rsync', 'it comes with macOS')

run('node', [path.join(root, 'scripts/check-rdp-assets.mjs')], root)

step(`Copying Tabby from ${tabby}`)
fs.mkdirSync(work, { recursive: true })
// --delete keeps the copy identical to Tabby; what's excluded (installed packages, build output, our two parts)
// stays between builds.
run('rsync', [
    '-a', '--delete',
    '--exclude', '.git', '--exclude', 'node_modules', '--exclude', '/*/dist/', '--exclude', '/*/typings/',
    '--exclude', '/tabby-rdp/', '--exclude', '/tabby-tauri/', '--exclude', '/tauri-page/',
    `${tabby}/`, `${work}/`,
], root)
run('rsync', ['-a', '--delete', '--exclude', 'node_modules', '--exclude', 'dist', `${root}/plugin/`, `${work}/tabby-tauri/`], root)
run('rsync', ['-a', '--delete', '--exclude', 'node_modules', '--exclude', 'dist', `${root}/rdp-plugin/`, `${work}/tabby-rdp/`], root)
run('rsync', ['-a', '--delete', '--exclude', 'node_modules', '--exclude', 'dist', `${root}/page/`, `${work}/tauri-page/`], root)

step('Installing packages')
// Tabby's Electron and its native modules aren't used: no Electron download, no native builds.
const installEnv = { ELECTRON_SKIP_BINARY_DOWNLOAD: '1', npm_config_ignore_scripts: 'true' }
const yarnInstall = (dir) => {
    const frozen = fs.existsSync(path.join(work, dir, 'yarn.lock')) ? ['--frozen-lockfile'] : []
    run('yarn', ['install', ...frozen, '--ignore-scripts', '--non-interactive', '--network-timeout', '1000000'], path.join(work, dir), installEnv)
}
const patchPackage = (dir) => {
    if (fs.existsSync(path.join(work, dir, 'patches'))) {
        run('node', [path.join(work, 'node_modules/patch-package/index.js')], path.join(work, dir))
    }
}
yarnInstall('.')
patchPackage('.')
for (const dir of ['app', 'web']) {
    yarnInstall(dir)
    patchPackage(dir)
}
for (const name of [...PLUGINS, 'tauri-page']) {
    yarnInstall(name)
}
// The lock files for our two parts, the first time they're made, go back into this project.
for (const [from, to] of [['tabby-rdp', 'rdp-plugin'], ['tabby-tauri', 'plugin'], ['tauri-page', 'page']]) {
    const lock = path.join(work, from, 'yarn.lock')
    if (fs.existsSync(lock) && !fs.existsSync(path.join(root, to, 'yarn.lock'))) {
        fs.copyFileSync(lock, path.join(root, to, 'yarn.lock'))
    }
}
// As Tabby's own install does on macOS and Linux: each plugin is a package the others can import.
for (const name of PLUGINS) {
    const link = path.join(work, 'node_modules', name)
    fs.rmSync(link, { recursive: true, force: true })
    fs.symlinkSync(`../${name}`, link)
}

step('Building Tabby\'s plugins, the Tauri plugin and the page')
const requireFromWork = createRequire(path.join(work, 'package.json'))
const webpack = requireFromWork('webpack')
async function build (configPath) {
    const config = (await import(url.pathToFileURL(configPath).href)).default()
    if (!process.env.TABBY_DEV && !configPath.includes('/tabby-rdp/')) {
        lighten(config)
    }
    const stats = await promisify(webpack)(config)
    if (stats.hasErrors()) {
        console.error(stats.toString({ colors: true, all: false, errors: true, warnings: false }))
        process.exit(1)
    }
}
/** A lighter page than Tabby's own builds (their configs are only changed here, in memory): scripts minified (class
 * names kept: some code reads them), no CSS source maps (css-loader would put them, with the SCSS sources, into the
 * styles the scripts carry), and Angular's development checks left out. TABBY_DEV=1 builds as Tabby does. */
function lighten (config) {
    config.optimization = {
        ...config.optimization,
        minimize: true,
        minimizeOptions: { javascript: { keep_classnames: true, compress: { passes: 1 } } },
    }
    for (const rule of config.module?.rules ?? []) {
        if (Array.isArray(rule.use)) {
            rule.use = rule.use.map(use => {
                const loader = typeof use === 'string' ? use : use.loader
                if (!['css-loader', 'sass-loader'].includes(loader)) {
                    return use
                }
                return { ...typeof use === 'string' ? {} : use, loader, options: { ...use.options, sourceMap: false } }
            })
        }
    }
    config.plugins = [...config.plugins ?? [], new webpack.DefinePlugin({ ngDevMode: false })]
}
for (const name of PLUGINS) {
    console.log(name)
    await build(path.join(work, name, 'webpack.config.mjs'))
}
console.log('page')
await build(path.join(work, 'tauri-page/webpack.config.mjs'))

fs.rmSync(page, { recursive: true, force: true })
fs.mkdirSync(page, { recursive: true })
fs.copyFileSync(path.join(work, 'tauri-page/index.html'), path.join(page, 'index.html'))
fs.cpSync(path.join(work, 'tauri-page/dist'), page, { recursive: true })
for (const name of PLUGINS) {
    const target = path.join(page, 'plugins', name)
    fs.mkdirSync(target, { recursive: true })
    fs.copyFileSync(path.join(work, name, 'package.json'), path.join(target, 'package.json'))
    fs.cpSync(path.join(work, name, 'dist'), path.join(target, 'dist'), { recursive: true })
    if (name === 'tabby-rdp') {
        fs.cpSync(path.join(work, name, 'vendor'), path.join(target, 'vendor'), { recursive: true })
        fs.copyFileSync(path.join(work, name, 'LICENSE'), path.join(target, 'LICENSE'))
    }
}
fs.writeFileSync(path.join(page, 'plugins.json'), JSON.stringify(PLUGINS, null, 2) + '\n')

step('Building the app with Tauri')
yarnInstallHere()
// The Rust version in rust-toolchain.toml, asked for explicitly (older rustups don't read that file).
const toolchain = /channel\s*=\s*"([^"]+)"/.exec(fs.readFileSync(path.join(root, 'rust-toolchain.toml'), 'utf-8'))[1]
try {
    execFileSync('rustup', ['run', toolchain, 'rustc', '--version'], { stdio: 'ignore' })
} catch {
    run('rustup', ['toolchain', 'install', toolchain, '--profile', 'minimal'], root)
}
process.env.RUSTUP_TOOLCHAIN = toolchain
// The app's icon is Tabby's.
const icon = path.join(tabby, 'build/icons/Icon-MacOS-512x512@2x.png')
run(path.join(root, 'node_modules/.bin/tauri'), ['icon', icon, '-o', path.join(root, 'src-tauri/icons')], root)
run(path.join(root, 'node_modules/.bin/tauri'), ['build', '--bundles', 'app', ...debug ? ['--debug'] : []], root)

const bundle = path.join(process.env.CARGO_TARGET_DIR ? path.resolve(root, process.env.CARGO_TARGET_DIR) : path.join(root, 'src-tauri/target'), debug ? 'debug' : 'release', 'bundle/macos/tabbi.app')
const out = path.join(root, 'out/tabbi.app')
fs.rmSync(out, { recursive: true, force: true })
fs.mkdirSync(path.dirname(out), { recursive: true })
run('ditto', [bundle, out], root)
console.log(`\ntabbi is born: ${out}`)

function yarnInstallHere () {
    const frozen = fs.existsSync(path.join(root, 'yarn.lock')) ? ['--frozen-lockfile'] : []
    run('yarn', ['install', ...frozen, '--non-interactive'], root)
}
