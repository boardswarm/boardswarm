// xterm.js glue code for Dioxus/WASM integration
// This file provides the bridge between Rust/WASM and the xterm.js library

const terminals = [];

export function xterm_create(elementId) {
    const container = document.getElementById(elementId);
    if (!container) {
        console.error(`xterm_create: element '${elementId}' not found`);
        return 0;
    }

    const term = new Terminal({
        cursorBlink: true,
        theme: {
            background: '#1a1a2e',
            foreground: '#eee',
            cursor: '#e94560',
        },
        fontSize: 14,
        fontFamily: '"Fira Code", "Cascadia Code", monospace',
        scrollback: 5000,
    });

    term.open(container);
    term.focus();

    const handle = terminals.length;
    terminals.push({ term, callbacks: {} });
    return handle;
}

export function xterm_write(handle, data) {
    const entry = terminals[handle];
    if (entry && entry.term) {
        // Convert Uint8Array to string
        const decoder = new TextDecoder();
        entry.term.write(decoder.decode(data));
    }
}

export function xterm_on_data(handle, callback) {
    const entry = terminals[handle];
    if (entry && entry.term) {
        entry.term.onData((data) => {
            callback(data);
        });
    }
}

export function xterm_dispose(handle) {
    const entry = terminals[handle];
    if (entry && entry.term) {
        entry.term.dispose();
        terminals[handle] = null;
    }
}
