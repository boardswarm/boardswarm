use boardswarm_protocol::{ConsoleInputRequest, ConsoleOutput, console_input_request};
use prost::Message;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, MessageEvent, WebSocket};

/// WebSocket-based console connection using protobuf framing.
pub struct ConsoleWs {
    ws: WebSocket,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut()>,
}

impl ConsoleWs {
    /// Open a WebSocket connection to the console endpoint.
    /// `console_id` is the console to connect to.
    /// `token` is the JWT for authentication.
    /// `on_output` is called with decoded console output bytes.
    pub fn connect(
        console_id: u64,
        token: &str,
        on_output: impl Fn(Vec<u8>) + 'static,
        on_close: impl Fn() + 'static,
    ) -> Result<Self, String> {
        let origin = web_sys::window()
            .unwrap()
            .location()
            .origin()
            .unwrap_or_else(|_| "http://localhost:6683".to_string());

        // Convert http(s) to ws(s)
        let ws_origin = origin
            .replace("https://", "wss://")
            .replace("http://", "ws://");

        let url = format!("{ws_origin}/api/ws/console?token={token}");
        let ws = WebSocket::new(&url).map_err(|e| format!("WebSocket open failed: {e:?}"))?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        // Send initial console selection message once connected
        let ws_clone = ws.clone();
        let on_open = Closure::once(move || {
            let msg = ConsoleInputRequest {
                target_or_data: Some(console_input_request::TargetOrData::Console(console_id)),
            };
            let data = msg.encode_to_vec();
            let _ = ws_clone.send_with_u8_array(&data);
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();

        // Handle incoming messages
        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = js_sys::Uint8Array::new(&buf);
                let data = array.to_vec();
                if let Ok(output) = ConsoleOutput::decode(data.as_slice()) {
                    on_output(output.data);
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        // Handle close
        let on_close_cb = Closure::wrap(Box::new(move || {
            on_close();
        }) as Box<dyn FnMut()>);
        ws.set_onclose(Some(on_close_cb.as_ref().unchecked_ref()));

        Ok(Self {
            ws,
            _on_message: on_message,
            _on_close: on_close_cb,
        })
    }

    /// Send input data to the console
    pub fn send_input(&self, data: Vec<u8>) -> Result<(), String> {
        let msg = ConsoleInputRequest {
            target_or_data: Some(console_input_request::TargetOrData::Data(data.into())),
        };
        let encoded = msg.encode_to_vec();
        self.ws
            .send_with_u8_array(&encoded)
            .map_err(|e| format!("WebSocket send failed: {e:?}"))
    }

    /// Close the WebSocket connection
    pub fn close(&self) {
        let _ = self.ws.close();
    }
}

impl Drop for ConsoleWs {
    fn drop(&mut self) {
        let _ = self.ws.close();
    }
}
