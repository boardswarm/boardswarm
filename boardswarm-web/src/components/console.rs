use dioxus::prelude::*;
use wasm_bindgen::prelude::*;

// JS interop for xterm.js
#[wasm_bindgen(module = "/src/xterm_glue.js")]
extern "C" {
    /// Create a new xterm.js Terminal and attach it to the given element ID.
    /// Returns a handle (index) to reference this terminal later.
    fn xterm_create(element_id: &str) -> u32;

    /// Write data to the terminal
    fn xterm_write(handle: u32, data: &[u8]);

    /// Register the on_data callback (called when user types)
    fn xterm_on_data(handle: u32, callback: &Closure<dyn FnMut(String)>);

    /// Dispose the terminal
    fn xterm_dispose(handle: u32);
}

#[component]
pub fn ConsoleView(console_id: u64, token: String) -> Element {
    let mut connected = use_signal(|| false);
    let mut ws_handle = use_signal(|| None::<crate::ws::ConsoleWs>);

    let container_id = format!("terminal-{console_id}");
    let container_id_clone = container_id.clone();

    // Set up terminal and WebSocket after mount
    use_effect(move || {
        let token = token.clone();
        let container_id = container_id_clone.clone();

        spawn(async move {
            // Small delay to ensure DOM element exists
            gloo_timers::future::TimeoutFuture::new(100).await;

            // Create xterm.js terminal
            let term_handle = xterm_create(&container_id);

            // Connect WebSocket
            let term_h = term_handle;
            let ws = crate::ws::ConsoleWs::connect(
                console_id,
                &token,
                move |data| {
                    xterm_write(term_h, &data);
                },
                move || {
                    connected.set(false);
                },
            );

            match ws {
                Ok(ws) => {
                    // Register input handler
                    let ws_ref = &ws;
                    let input_closure = Closure::wrap(Box::new(move |data: String| {
                        // TODO: need to send via ws - for now this is a placeholder
                        // The closure needs access to the ws handle
                        let _ = data;
                    })
                        as Box<dyn FnMut(String)>);
                    xterm_on_data(term_handle, &input_closure);
                    input_closure.forget();

                    ws_handle.set(Some(ws));
                    connected.set(true);
                }
                Err(e) => {
                    tracing::error!("WebSocket connection failed: {e}");
                }
            }
        });
    });

    rsx! {
        div {
            if !connected() {
                p { style: "color: #888;", "Connecting..." }
            }
            div {
                id: "{container_id}",
                class: "terminal-container",
            }
        }
    }
}
