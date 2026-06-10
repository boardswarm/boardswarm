use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen::prelude::*;

// JS interop for xterm.js
#[wasm_bindgen(module = "/src/xterm_glue.js")]
extern "C" {
    fn xterm_create(element_id: &str) -> u32;
    fn xterm_write(handle: u32, data: &[u8]);
    fn xterm_on_data(handle: u32, callback: &Closure<dyn FnMut(String)>);
    fn xterm_dispose(handle: u32);
}

#[component]
pub fn ConsoleView(console_id: u64, token: String) -> Element {
    let mut connected = use_signal(|| false);
    let mut status_msg = use_signal(|| "Connecting...".to_string());

    let container_id = format!("terminal-{console_id}");
    let container_id_clone = container_id.clone();

    use_effect(move || {
        let token = token.clone();
        let container_id = container_id_clone.clone();

        spawn(async move {
            // Small delay to ensure DOM element exists
            gloo_timers::future::TimeoutFuture::new(100).await;

            let term_handle = xterm_create(&container_id);

            // Connect WebSocket
            let write_handle = term_handle;
            let ws = crate::ws::ConsoleWs::connect(
                console_id,
                &token,
                move |data| {
                    xterm_write(write_handle, &data);
                },
                move || {
                    connected.set(false);
                    status_msg.set("Disconnected".to_string());
                },
            );

            match ws {
                Ok(ws) => {
                    let ws = Rc::new(ws);
                    let ws_for_input = ws.clone();

                    // Register xterm.js input handler -> send to WebSocket
                    let input_closure = Closure::wrap(Box::new(move |data: String| {
                        let _ = ws_for_input.send_input(data.into_bytes());
                    })
                        as Box<dyn FnMut(String)>);
                    xterm_on_data(term_handle, &input_closure);
                    input_closure.forget();

                    // Keep ws alive for the session lifetime
                    std::mem::forget(ws);

                    connected.set(true);
                    status_msg.set("Connected".to_string());
                }
                Err(e) => {
                    status_msg.set(format!("Connection failed: {e}"));
                    tracing::error!("WebSocket connection failed: {e}");
                }
            }
        });
    });

    rsx! {
        div {
            p {
                style: if connected() { "color: #4caf50; font-size: 0.85rem;" } else { "color: #888; font-size: 0.85rem;" },
                "{status_msg}"
            }
            div {
                id: "{container_id}",
                class: "terminal-container",
            }
        }
    }
}
