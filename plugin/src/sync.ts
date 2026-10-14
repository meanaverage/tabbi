// A synchronous answer from the Rust side, for the few calls Tabby expects to return right away (see
// src-tauri/src/sync.rs; the page has the same helper in page/src/node/sync.ts).
export function syncCall<T> (call: string, args: Record<string, any> = {}): T {
    const request = new XMLHttpRequest()
    request.open('POST', `tabbysync://localhost/${call}`, false)
    request.setRequestHeader('Content-Type', 'text/plain')
    request.send(JSON.stringify(args))
    if (request.status !== 200) {
        throw new Error(request.responseText || `${call} failed (${request.status})`)
    }
    return JSON.parse(request.responseText)
}
