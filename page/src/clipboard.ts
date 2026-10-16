import { invoke } from '@tauri-apps/api/core'

const supportedTypes = new Set(['text/plain', 'text/html', 'image/png'])
let cachedChange: string | undefined
let cachedItems: ClipboardItem[] = []
let pending: Promise<unknown> = Promise.resolve()

// Keep writes (including asynchronous ClipboardItem data) and reads in call order.
function ordered<T> (operation: () => Promise<T>): Promise<T> {
    const result = pending.then(operation)
    pending = result.catch(() => {})
    return result
}

async function read (): Promise<ClipboardItem[]> {
    const change = await invoke<string>('clipboard_change_count')
    if (change !== cachedChange) {
        const bytes = await invoke<ArrayBuffer>('clipboard_read_items')
        const length = new DataView(bytes).getUint32(0, true)
        const header = JSON.parse(new TextDecoder().decode(new Uint8Array(bytes, 4, length)))
        const data: Record<string, Blob> = {}
        for (const [key, type] of [['text', 'text/plain'], ['html', 'text/html']]) {
            if (header[key] !== null) {
                data[type] = new Blob([header[key]], { type })
            }
        }
        if (bytes.byteLength > 4 + length) {
            data['image/png'] = new Blob([new Uint8Array(bytes, 4 + length)], { type: 'image/png' })
        }
        cachedItems = Object.keys(data).length ? [new ClipboardItem(data)] : []
        cachedChange = header.change
    }
    // The items/Blobs are immutable, but callers may change the returned array.
    return cachedItems.slice()
}

async function write (items: ClipboardItem[]): Promise<void> {
    if (!items.length) {
        return
    }
    if (items.length > 1) {
        throw new DOMException('Only one clipboard item is supported', 'NotSupportedError')
    }
    const item = items[0]
    for (const type of item.types) {
        if (!supportedTypes.has(type)) {
            throw new DOMException(`Unsupported clipboard type: ${type}`, 'NotSupportedError')
        }
    }
    const header = { text: null as string | null, html: null as string | null }
    let png = new Uint8Array(0)
    for (const type of item.types) {
        const blob = await item.getType(type)
        if (type === 'image/png') {
            png = new Uint8Array(await blob.arrayBuffer())
        } else {
            header[type === 'text/plain' ? 'text' : 'html'] = await blob.text()
        }
    }
    const json = new TextEncoder().encode(JSON.stringify(header))
    const bytes = new Uint8Array(4 + json.length + png.length)
    new DataView(bytes.buffer).setUint32(0, json.length, true)
    bytes.set(json, 4)
    bytes.set(png, 4 + json.length)
    // Invalidate before IPC: even a failed write may have changed native ownership.
    cachedChange = undefined
    await invoke('clipboard_write_items', bytes)
}

// Installed by preload before plugins (notably IronRDP's 100 ms clipboard monitor).
// Keep the native Clipboard object, including its EventTarget behavior.
const clipboard = navigator.clipboard ?? new EventTarget()
if (!navigator.clipboard) {
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: clipboard })
}
Object.defineProperties(clipboard, {
    read: { configurable: true, writable: true, value: () => ordered(read) },
    readText: { configurable: true, writable: true, value: () => ordered(() => invoke<string>('clipboard_read')) },
    write: { configurable: true, writable: true, value: (items: ClipboardItem[]) => ordered(() => write(items)) },
    writeText: {
        configurable: true,
        writable: true,
        value: (text: string) => ordered(async () => {
            cachedChange = undefined
            await invoke('clipboard_write', { text: String(text), html: null })
        }),
    },
})
