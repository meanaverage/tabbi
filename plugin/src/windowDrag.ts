// Moving the window by its title bar. Tabby marks the parts of its window that drag it (and the parts inside those
// that don't) with Electron's `-webkit-app-region: drag | no-drag`. WebKit doesn't know that property and drops it
// from the styles it applies, but it is still in the text of Tabby's <style> elements, so the marks are read from
// there: whatever Tabby marks, now or in a later version, drags the window here too. Tabby adds styles as its parts
// first appear (title bar, profile list…), so new <style> elements are read as they come.
//
// As in Electron, the innermost marked element under the pointer decides; a `::before`/`::after` mark covers only that
// pseudo-element's box (Tabby's strip above an open dialog). A double click is left to the page.

interface DragRule {
    selector: string
    pseudo: string|null
    drag: boolean
}

const rules: DragRule[] = []
const read = new WeakSet<HTMLStyleElement>()

function readStyle (style: HTMLStyleElement): void {
    read.add(style)
    const text = style.textContent ?? ''
    let i = text.indexOf('app-region')
    while (i >= 0) {
        const kind = /^app-region\s*:\s*(no-drag|drag)/.exec(text.slice(i, i + 40))?.[1]
        const open = text.lastIndexOf('{', i)
        // (after the rule before it, or the start of an @media block it's in)
        const selectorText = text.slice(Math.max(text.lastIndexOf('}', open - 1), text.lastIndexOf('{', open - 1)) + 1, open)
        if (kind && !selectorText.includes('@')) {
            for (let selector of selectorText.split(',').map(s => s.trim()).filter(Boolean)) {
                const pseudo = /::?(before|after)$/.exec(selector)?.[0] ?? null
                if (pseudo) {
                    selector = selector.slice(0, -pseudo.length)
                }
                try {
                    document.querySelector(selector)
                    rules.push({ selector, pseudo: pseudo && `::${pseudo.replace(/^:+/, '')}`, drag: kind === 'drag' })
                } catch {
                    // not a selector this browser understands
                }
            }
        }
        i = text.indexOf('app-region', i + 10)
    }
}

function readNewStyles (): void {
    for (const style of Array.from(document.querySelectorAll('style'))) {
        if (!read.has(style)) {
            readStyle(style)
        }
    }
}

function insidePseudo (element: Element, pseudo: string, x: number, y: number): boolean {
    const style = getComputedStyle(element, pseudo)
    if (style.content === 'none' || style.display === 'none') {
        return false
    }
    const box = style.position === 'fixed'
        ? { left: 0, top: 0, width: window.innerWidth, height: window.innerHeight }
        : element.getBoundingClientRect()
    const px = (v: string) => parseFloat(v) || 0
    const left = box.left + px(style.left)
    const right = style.right === 'auto' ? left + px(style.width) : box.left + box.width - px(style.right)
    const top = box.top + px(style.top)
    const bottom = style.bottom === 'auto' ? top + px(style.height) : box.top + box.height - px(style.bottom)
    return x >= left && x < right && y >= top && y < bottom
}

/** Whether a press at this point, on this element, should move the window. */
export function startsWindowDrag (target: Element|null, x: number, y: number): boolean {
    readNewStyles()
    for (let element = target; element; element = element.parentElement) {
        let decided: boolean|null = null
        for (const rule of rules) {
            if (element.matches(rule.selector) && (!rule.pseudo || insidePseudo(element, rule.pseudo, x, y))) {
                // On one element, a no-drag mark wins.
                decided = decided === false ? false : rule.drag
            }
        }
        if (decided !== null) {
            return decided
        }
    }
    return false
}

export function followTabbyDragRegions (startDragging: () => void): void {
    document.addEventListener('mousedown', event => {
        if (event.button === 0 && event.detail === 1 && startsWindowDrag(event.target as Element|null, event.clientX, event.clientY)) {
            startDragging()
        }
    })
}
