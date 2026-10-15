import { MenuItemOptions, PlatformService, TranslateService } from 'tabby-core'

const terminalInput = '.xterm-helper-textarea, textarea[aria-label="Terminal input"]'
const textInputs = 'input:not([type]), input[type=text], input[type=search], input[type=password], input[type=email], input[type=url], input[type=tel], input[type=number], textarea'

/** The innermost element an event started on. */
function targetElement (event: Event): HTMLElement | null {
    const node = event.composedPath()[0] as Node | undefined
    return node?.nodeType === Node.ELEMENT_NODE ? node as HTMLElement : null
}

/**
 * Right clicks outside Tabby's own menus. WebKit's menu (with Reload, which would lose every tab) never opens; text
 * fields get Undo, Cut, Copy, Paste and Select All, and selected text elsewhere gets Copy, through Tabby's clipboard.
 * The context menu key and Shift-F10 open the same menus from the keyboard.
 */
export class TextFieldMenus {
    constructor (private platform: PlatformService, private translate: TranslateService) { }

    install (doc: Document): void {
        doc.addEventListener('contextmenu', event => {
            // (capturing, so it also covers controls and overlays that stop propagation themselves)
            event.preventDefault()
            const target = targetElement(event)
            if (!target || target.closest(terminalInput)) {
                return
            }
            const field = target.closest<HTMLElement>(textInputs + ', [contenteditable]')
            if (field && (field.matches(textInputs) || field.isContentEditable)) {
                event.stopPropagation()
                this.editMenu(field, event)
            }
        }, true)
        doc.addEventListener('contextmenu', event => {
            // Tabby's own menus stop propagation; selected text that none of them claimed gets Copy.
            const selection = doc.getSelection()
            const target = event.composedPath()[0] as Node
            if (selection?.toString() && selection.containsNode(target, true)) {
                const text = selection.toString()
                this.platform.popupContextMenu([{ label: this.translate.instant('Copy'), click: () => this.platform.setClipboard({ text }) }], event)
            }
        })
        doc.addEventListener('keydown', event => {
            if (event.key !== 'ContextMenu' && !(event.key === 'F10' && event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey)) {
                return
            }
            const target = targetElement(event)
            // Terminals receive these keys themselves; with nothing focused there's no menu.
            if (!target || target === doc.body || target === doc.documentElement || target.closest(terminalInput)) {
                return
            }
            event.preventDefault()
            event.stopPropagation()
            const rect = target.getBoundingClientRect()
            target.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, composed: true, cancelable: true, clientX: rect.left + 8, clientY: rect.bottom }))
        }, true)
    }

    /** Also for tests: the items the menu for this field would have. */
    editItems (field: HTMLElement): MenuItemOptions[] {
        const doc = field.ownerDocument
        const input = field.matches(textInputs) ? field as HTMLInputElement | HTMLTextAreaElement : null
        const disabled = input?.matches(':disabled') ?? false
        const writable = !disabled && !input?.readOnly
        const password = input?.type === 'password'
        if (!disabled) {
            field.focus({ preventScroll: true })
        }
        const start = input?.selectionStart ?? 0
        const end = input?.selectionEnd ?? 0
        const selection = doc.getSelection()
        const range = !input && selection?.rangeCount && field.contains(selection.anchorNode) ? selection.getRangeAt(0).cloneRange() : null
        const text = input ? input.value.slice(start, end) : range?.toString() ?? ''
        // The menu takes focus away: put it and the selection back before acting.
        const restore = (): boolean => {
            if (!field.isConnected || disabled) {
                return false
            }
            field.focus({ preventScroll: true })
            if (input?.selectionStart != null) {
                input.setSelectionRange(start, end)
            }
            if (range && selection) {
                selection.removeAllRanges()
                selection.addRange(range)
            }
            return true
        }
        const execute = (command: string, value?: string): void => {
            if (restore()) {
                doc.execCommand(command, false, value)
            }
        }
        let clipboard = ''
        try {
            clipboard = this.platform.readClipboard()
        } catch {
            // no clipboard text
        }
        const item = (label: string, enabled: boolean, click: () => void): MenuItemOptions => ({ label: this.translate.instant(label), enabled, click })
        return [
            item('Undo', writable && doc.queryCommandEnabled('undo'), () => execute('undo')),
            item('Redo', writable && doc.queryCommandEnabled('redo'), () => execute('redo')),
            { type: 'separator' },
            item('Cut', writable && !password && !!text, () => {
                this.platform.setClipboard({ text })
                execute('delete')
            }),
            item('Copy', !disabled && !password && !!text, () => this.platform.setClipboard({ text })),
            item('Paste', writable && !!clipboard, () => execute('insertText', clipboard)),
            { type: 'separator' },
            item('Select All', !disabled && !!(input ? input.value : field.textContent), () => {
                if (!restore()) {
                    return
                }
                if (input) {
                    input.select()
                } else {
                    const all = doc.createRange()
                    all.selectNodeContents(field)
                    selection?.removeAllRanges()
                    selection?.addRange(all)
                }
            }),
        ]
    }

    private editMenu (field: HTMLElement, event: MouseEvent): void {
        this.platform.popupContextMenu(this.editItems(field), event)
    }
}
