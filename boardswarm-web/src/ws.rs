use std::cell::RefCell;
use std::rc::Rc;

use boardswarm_protocol::{
    ConsoleInputRequest, ConsoleOutput, KeyboardRequest, KeyboardState, MediaRequest, MouseRequest,
    SignalMessage, SignalMessageIceCandidate, SignalMessageSdp, console_input_request,
    keyboard_request, media_request, mouse_request, signal_message,
};
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
        on_output: impl FnMut(Vec<u8>) + 'static,
        on_close: impl FnMut() + 'static,
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
        let on_output = Rc::new(RefCell::new(on_output));
        let on_output_clone = on_output.clone();
        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = js_sys::Uint8Array::new(&buf);
                let data = array.to_vec();
                if let Ok(output) = ConsoleOutput::decode(data.as_slice()) {
                    (on_output_clone.borrow_mut())(output.data.to_vec());
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        // Handle close
        let on_close = Rc::new(RefCell::new(on_close));
        let on_close_clone = on_close.clone();
        let on_close_cb = Closure::wrap(Box::new(move || {
            (on_close_clone.borrow_mut())();
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
    #[allow(dead_code)]
    pub fn close(&self) {
        let _ = self.ws.close();
    }
}

impl Drop for ConsoleWs {
    fn drop(&mut self) {
        let _ = self.ws.close();
    }
}

/// WebSocket-based media signaling connection using protobuf framing.
///
/// Sends [`MediaRequest`] protobuf frames to the server and receives
/// [`SignalMessage`] protobuf frames (SDP offer, ICE candidates) from it.
pub struct MediaWs {
    ws: WebSocket,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut()>,
}

impl MediaWs {
    /// Open a WebSocket connection to the media signaling endpoint.
    ///
    /// `on_offer` is called with the SDP offer string from the server.
    /// `on_ice` is called with (candidate, mline_index) for each ICE candidate from the server.
    /// `on_close` is called when the connection is closed.
    pub fn connect(
        media_id: u64,
        token: &str,
        mut on_offer: impl FnMut(String) + 'static,
        mut on_ice: impl FnMut(String, u32) + 'static,
        on_close: impl FnMut() + 'static,
    ) -> Result<Self, String> {
        let origin = web_sys::window()
            .unwrap()
            .location()
            .origin()
            .unwrap_or_else(|_| "http://localhost:6683".to_string());

        let ws_origin = origin
            .replace("https://", "wss://")
            .replace("http://", "ws://");

        let url = format!("{ws_origin}/api/ws/media?token={token}");
        let ws = WebSocket::new(&url).map_err(|e| format!("WebSocket open failed: {e:?}"))?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        // Send initial media selection message once connected
        let ws_clone = ws.clone();
        let on_open = Closure::once(move || {
            let msg = MediaRequest {
                item_or_signal: Some(media_request::ItemOrSignal::Item(media_id)),
            };
            let _ = ws_clone.send_with_u8_array(&msg.encode_to_vec());
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();

        // Handle incoming signal messages
        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = js_sys::Uint8Array::new(&buf);
                let data = array.to_vec();
                if let Ok(signal) = SignalMessage::decode(data.as_slice()) {
                    match signal.sdp_message {
                        Some(signal_message::SdpMessage::Offer(sdp)) => on_offer(sdp.sdp),
                        Some(signal_message::SdpMessage::Ice(ice)) => {
                            on_ice(ice.candidate, ice.mline_index)
                        }
                        Some(signal_message::SdpMessage::Answer(_)) => {
                            // Browser is always the answerer; server should never send an answer
                        }
                        None => {}
                    }
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        // Handle close
        let on_close = Rc::new(RefCell::new(on_close));
        let on_close_clone = on_close.clone();
        let on_close_cb = Closure::wrap(Box::new(move || {
            (on_close_clone.borrow_mut())();
        }) as Box<dyn FnMut()>);
        ws.set_onclose(Some(on_close_cb.as_ref().unchecked_ref()));

        Ok(Self {
            ws,
            _on_message: on_message,
            _on_close: on_close_cb,
        })
    }

    /// Send an SDP answer to the server.
    pub fn send_answer(&self, sdp: String) -> Result<(), String> {
        let msg = MediaRequest {
            item_or_signal: Some(media_request::ItemOrSignal::Signal(SignalMessage {
                sdp_message: Some(signal_message::SdpMessage::Answer(SignalMessageSdp { sdp })),
            })),
        };
        self.ws
            .send_with_u8_array(&msg.encode_to_vec())
            .map_err(|e| format!("WebSocket send failed: {e:?}"))
    }

    /// Send a local ICE candidate to the server.
    pub fn send_ice(&self, candidate: String, mline_index: u32) -> Result<(), String> {
        let msg = MediaRequest {
            item_or_signal: Some(media_request::ItemOrSignal::Signal(SignalMessage {
                sdp_message: Some(signal_message::SdpMessage::Ice(SignalMessageIceCandidate {
                    candidate,
                    mline_index,
                })),
            })),
        };
        self.ws
            .send_with_u8_array(&msg.encode_to_vec())
            .map_err(|e| format!("WebSocket send failed: {e:?}"))
    }
}

impl Drop for MediaWs {
    fn drop(&mut self) {
        let _ = self.ws.close();
    }
}

fn ws_url(path: &str, token: &str) -> Result<String, String> {
    let origin = web_sys::window()
        .unwrap()
        .location()
        .origin()
        .unwrap_or_else(|_| "http://localhost:6683".to_string());
    let ws_origin = origin
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    Ok(format!("{ws_origin}{path}?token={token}"))
}

/// WebSocket connection for keyboard input / LED state output.
pub struct KeyboardWs {
    ws: WebSocket,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut()>,
}

impl KeyboardWs {
    /// Open a keyboard WebSocket.
    ///
    /// `on_state` is called whenever a `KeyboardState` (LED report) arrives.
    /// `on_close` is called when the connection closes.
    pub fn connect(
        keyboard_id: u64,
        token: &str,
        mut on_state: impl FnMut(KeyboardState) + 'static,
        on_close: impl FnMut() + 'static,
    ) -> Result<Self, String> {
        let url = ws_url("/api/ws/keyboard", token)?;
        let ws = WebSocket::new(&url).map_err(|e| format!("WebSocket open failed: {e:?}"))?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let ws_clone = ws.clone();
        let on_open = Closure::once(move || {
            let msg = KeyboardRequest {
                item_or_signal: Some(keyboard_request::ItemOrSignal::Item(keyboard_id)),
            };
            let _ = ws_clone.send_with_u8_array(&msg.encode_to_vec());
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();

        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = js_sys::Uint8Array::new(&buf);
                if let Ok(state) = KeyboardState::decode(array.to_vec().as_slice()) {
                    on_state(state);
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let on_close = Rc::new(RefCell::new(on_close));
        let on_close_clone = on_close.clone();
        let on_close_cb = Closure::wrap(Box::new(move || {
            (on_close_clone.borrow_mut())();
        }) as Box<dyn FnMut()>);
        ws.set_onclose(Some(on_close_cb.as_ref().unchecked_ref()));

        Ok(Self {
            ws,
            _on_message: on_message,
            _on_close: on_close_cb,
        })
    }

    /// Send a keyboard event (key down or key up).
    pub fn send_event(
        &self,
        event_type: boardswarm_protocol::KeyboardEventType,
        key: u32,
    ) -> Result<(), String> {
        let msg = KeyboardRequest {
            item_or_signal: Some(keyboard_request::ItemOrSignal::Event(
                boardswarm_protocol::KeyboardEvent {
                    r#type: event_type as i32,
                    key,
                },
            )),
        };
        self.ws
            .send_with_u8_array(&msg.encode_to_vec())
            .map_err(|e| format!("WebSocket send failed: {e:?}"))
    }
}

impl Drop for KeyboardWs {
    fn drop(&mut self) {
        let _ = self.ws.close();
    }
}

/// WebSocket connection for sending mouse input events to the server.
pub struct MouseWs {
    ws: WebSocket,
}

impl MouseWs {
    /// Open a mouse WebSocket.
    pub fn connect(mouse_id: u64, token: &str) -> Result<Self, String> {
        let url = ws_url("/api/ws/mouse", token)?;
        let ws = WebSocket::new(&url).map_err(|e| format!("WebSocket open failed: {e:?}"))?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let ws_clone = ws.clone();
        let on_open = Closure::once(move || {
            let msg = MouseRequest {
                item_or_signal: Some(mouse_request::ItemOrSignal::Item(mouse_id)),
            };
            let _ = ws_clone.send_with_u8_array(&msg.encode_to_vec());
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();

        Ok(Self { ws })
    }

    /// Send a mouse input report with absolute position scaled to 0-32767.
    pub fn send_input(
        &self,
        buttons: u32,
        x: u32,
        y: u32,
        wheel: i32,
        hwheel: i32,
    ) -> Result<(), String> {
        let msg = MouseRequest {
            item_or_signal: Some(mouse_request::ItemOrSignal::Input(
                boardswarm_protocol::MouseInput {
                    buttons,
                    x,
                    y,
                    wheel,
                    hwheel,
                },
            )),
        };
        self.ws
            .send_with_u8_array(&msg.encode_to_vec())
            .map_err(|e| format!("WebSocket send failed: {e:?}"))
    }
}

impl Drop for MouseWs {
    fn drop(&mut self) {
        let _ = self.ws.close();
    }
}
