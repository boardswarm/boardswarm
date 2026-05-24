use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen::prelude::*;

use crate::ws::MediaWs;

// WebRTC JS glue functions
#[wasm_bindgen(module = "/src/webrtc_glue.js")]
extern "C" {
    fn webrtc_create(video_element_id: &str) -> u32;
    fn webrtc_on_ice(handle: u32, callback: &Closure<dyn FnMut(String, u32)>);
    fn webrtc_on_connected(handle: u32, callback: &Closure<dyn FnMut()>);
    fn webrtc_set_offer(handle: u32, offer_sdp: &str, on_answer: &Closure<dyn FnMut(String)>);
    fn webrtc_add_ice(handle: u32, candidate: &str, mline_index: u32);
    fn webrtc_dispose(handle: u32);
}

/// A component that streams video from a boardswarm media item via WebRTC.
///
/// Opens a WebSocket signaling connection to `/api/ws/media`, negotiates WebRTC
/// with the server (which acts as offerer), and renders the received stream into
/// a `<video>` element.
#[component]
pub fn MediaViewer(media_id: u64, token: String) -> Element {
    let mut status = use_signal(|| "Connecting...".to_string());
    let mut connected = use_signal(|| false);

    let video_id = format!("media-video-{media_id}");
    let video_id_clone = video_id.clone();

    use_effect(move || {
        let token = token.clone();
        let video_id = video_id_clone.clone();

        spawn(async move {
            // Small delay so the DOM element is ready before we reference it
            gloo_timers::future::TimeoutFuture::new(100).await;

            let pc_handle = webrtc_create(&video_id);

            // Shared reference to MediaWs so answer/ICE callbacks can use it
            let ws_shared: Rc<RefCell<Option<MediaWs>>> = Rc::new(RefCell::new(None));
            let ws_for_answer = ws_shared.clone();
            let ws_for_local_ice = ws_shared.clone();

            // Register local ICE candidate callback: forward to server via WebSocket
            let local_ice_cb = Closure::wrap(Box::new(move |candidate: String, mline: u32| {
                if let Some(ws) = ws_for_local_ice.borrow().as_ref() {
                    let _ = ws.send_ice(candidate, mline);
                }
            }) as Box<dyn FnMut(String, u32)>);
            webrtc_on_ice(pc_handle, &local_ice_cb);
            local_ice_cb.forget();

            // Register connected callback
            let connected_cb = Closure::once(move || {
                connected.set(true);
                status.set("Connected".to_string());
            });
            webrtc_on_connected(pc_handle, &connected_cb);
            connected_cb.forget();

            // Connect the WebSocket signaling channel
            let ws_result = MediaWs::connect(
                media_id,
                &token,
                // on_offer: server sent SDP offer → create answer → send back
                move |offer_sdp: String| {
                    let ws_cell = ws_for_answer.clone();
                    let on_answer_cb =
                        Closure::once(move |answer_sdp: String| {
                            if let Some(ws) = ws_cell.borrow().as_ref() {
                                let _ = ws.send_answer(answer_sdp);
                            }
                        });
                    webrtc_set_offer(pc_handle, &offer_sdp, &on_answer_cb);
                    on_answer_cb.forget();
                },
                // on_ice: ICE candidate from server → add to local peer connection
                move |candidate: String, mline_index: u32| {
                    webrtc_add_ice(pc_handle, &candidate, mline_index);
                },
                // on_close
                move || {
                    connected.set(false);
                    status.set("Disconnected".to_string());
                    webrtc_dispose(pc_handle);
                },
            );

            match ws_result {
                Ok(ws) => {
                    *ws_shared.borrow_mut() = Some(ws);
                }
                Err(e) => {
                    status.set(format!("Connection failed: {e}"));
                    webrtc_dispose(pc_handle);
                }
            }
        });
    });

    rsx! {
        div {
            p {
                style: if connected() {
                    "color: #4caf50; font-size: 0.85rem;"
                } else {
                    "color: #888; font-size: 0.85rem;"
                },
                "{status}"
            }
            video {
                id: "{video_id}",
                autoplay: "true",
                playsinline: "true",
                style: "width: 100%; max-width: 800px; background: #000; border-radius: 4px;",
            }
        }
    }
}
