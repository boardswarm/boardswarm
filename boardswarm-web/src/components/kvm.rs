use std::cell::RefCell;
use std::rc::Rc;

use boardswarm_protocol::KeyboardEventType;
use dioxus::html::geometry::WheelDelta;
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::kvm_input::browser_code_to_hid;
use crate::ws::{KeyboardWs, MediaWs, MouseWs};

// Reuse the same WebRTC JS glue from the media viewer
#[wasm_bindgen(module = "/src/webrtc_glue.js")]
extern "C" {
    fn webrtc_create(video_element_id: &str) -> u32;
    fn webrtc_on_ice(handle: u32, callback: &Closure<dyn FnMut(String, u32)>);
    fn webrtc_on_connected(handle: u32, callback: &Closure<dyn FnMut()>);
    fn webrtc_set_offer(handle: u32, offer_sdp: &str, on_answer: &Closure<dyn FnMut(String)>);
    fn webrtc_add_ice(handle: u32, candidate: &str, mline_index: u32);
    fn webrtc_dispose(handle: u32);
    fn webrtc_enter_fill(video_element_id: &str);
    fn webrtc_exit_fill(video_element_id: &str);
}

fn request_fullscreen(element_id: &str) {
    if let Some(window) = web_sys::window() {
        if let Some(document) = window.document() {
            if let Some(element) = document.get_element_by_id(element_id) {
                let _ = element.request_fullscreen();
            }
        }
    }
}

/// Map element-relative coordinates (ex, ey) to HID absolute range 0-32767,
/// accounting for letterboxing/pillarboxing from `object-fit: contain`.
///
/// When the video aspect ratio doesn't match the element's, the browser
/// renders black bars. We compute the actual rendered video rect, clamp the
/// cursor position to it, and scale only within that rect.
fn video_coords_to_hid(video_element_id: &str, ex: f64, ey: f64) -> Option<(u32, u32)> {
    let document = web_sys::window()?.document()?;
    let el = document.get_element_by_id(video_element_id)?;
    let video = el.dyn_ref::<web_sys::HtmlVideoElement>()?;

    let rect = video.get_bounding_client_rect();
    let el_w = rect.width();
    let el_h = rect.height();
    if el_w <= 0.0 || el_h <= 0.0 {
        return None;
    }

    let vid_w = video.video_width() as f64;
    let vid_h = video.video_height() as f64;

    // Compute the rendered content rect within the element (object-fit: contain).
    let (rendered_w, rendered_h, offset_x, offset_y) = if vid_w > 0.0 && vid_h > 0.0 {
        let el_ar = el_w / el_h;
        let vid_ar = vid_w / vid_h;
        if vid_ar > el_ar {
            // Wider than display box — bars on top and bottom (letterbox)
            let rw = el_w;
            let rh = el_w / vid_ar;
            (rw, rh, 0.0, (el_h - rh) / 2.0)
        } else {
            // Taller than display box — bars on left and right (pillarbox)
            let rh = el_h;
            let rw = el_h * vid_ar;
            (rw, rh, (el_w - rw) / 2.0, 0.0)
        }
    } else {
        // Video metadata not yet available — fall back to full element area.
        (el_w, el_h, 0.0, 0.0)
    };

    let rel_x = (ex - offset_x).clamp(0.0, rendered_w);
    let rel_y = (ey - offset_y).clamp(0.0, rendered_h);
    let x = ((rel_x / rendered_w) * 32767.0) as u32;
    let y = ((rel_y / rendered_h) * 32767.0) as u32;
    Some((x, y))
}

/// Build a mouse buttons bitmask from a Dioxus `MouseButtonSet`.
/// Bit 0 = primary (left), bit 1 = secondary (right), bit 2 = auxiliary (middle).
fn build_buttons(held: dioxus::html::input_data::MouseButtonSet) -> u32 {
    let mut buttons: u32 = 0;
    if held.contains(MouseButton::Primary) {
        buttons |= 1;
    }
    if held.contains(MouseButton::Secondary) {
        buttons |= 2;
    }
    if held.contains(MouseButton::Auxiliary) {
        buttons |= 4;
    }
    buttons
}

/// KVM viewer: WebRTC video + optional keyboard and mouse input capture.
///
/// Mouse is absolute: cursor position within the video element is scaled to
/// the 0–32767 HID range. Keyboard events are captured while the wrapper div
/// is focused (click the video area to focus).
#[component]
pub fn KvmViewer(
    media_id: u64,
    keyboard_id: Option<u64>,
    mouse_id: Option<u64>,
    token: String,
) -> Element {
    let mut status = use_signal(|| "Connecting...".to_string());
    let mut connected = use_signal(|| false);
    let mut led_names = use_signal(|| Vec::<String>::new());

    let video_id = format!("kvm-video-{media_id}");
    let container_id = format!("kvm-container-{media_id}");

    // Keyboard and mouse sessions — created once and held for the component lifetime.
    // use_hook ensures the Rc is not replaced on re-renders.
    let keyboard_ws = use_hook(|| Rc::new(RefCell::new(None::<KeyboardWs>)));
    let mouse_ws = use_hook(|| Rc::new(RefCell::new(None::<MouseWs>)));

    let keyboard_ws_keydown = keyboard_ws.clone();
    let keyboard_ws_keyup = keyboard_ws.clone();
    let mouse_ws_move = mouse_ws.clone();
    let mouse_ws_down = mouse_ws.clone();
    let mouse_ws_up = mouse_ws.clone();
    let mouse_ws_wheel = mouse_ws.clone();

    // Effect clones — re-cloned inside use_effect for FnMut compatibility
    let keyboard_ws_for_effect = keyboard_ws.clone();
    let mouse_ws_for_effect = mouse_ws.clone();
    let video_id_effect = video_id.clone();
    let token_effect = token.clone();

    use_effect(move || {
        // Clone Rc handles so this FnMut closure can be called more than once.
        let keyboard_ws_effect = keyboard_ws_for_effect.clone();
        let mouse_ws_effect = mouse_ws_for_effect.clone();
        let token = token_effect.clone();
        let video_id = video_id_effect.clone();

        spawn(async move {
            gloo_timers::future::TimeoutFuture::new(100).await;

            // Open keyboard WebSocket if requested
            if let Some(kb_id) = keyboard_id {
                match KeyboardWs::connect(
                    kb_id,
                    &token,
                    move |state| {
                        let names: Vec<String> = state
                            .led
                            .iter()
                            .filter_map(|v| {
                                boardswarm_protocol::KeyboardLed::try_from(*v)
                                    .ok()
                                    .map(|l| format!("{l:?}"))
                            })
                            .collect();
                        led_names.set(names);
                    },
                    || {},
                ) {
                    Ok(ws) => {
                        *keyboard_ws_effect.borrow_mut() = Some(ws);
                    }
                    Err(e) => tracing::error!("keyboard WebSocket failed: {e}"),
                }
            }

            // Open mouse WebSocket if requested
            if let Some(ms_id) = mouse_id {
                match MouseWs::connect(ms_id, &token) {
                    Ok(ws) => {
                        *mouse_ws_effect.borrow_mut() = Some(ws);
                    }
                    Err(e) => tracing::error!("mouse WebSocket failed: {e}"),
                }
            }

            // Set up WebRTC video (same flow as MediaViewer)
            let pc_handle = webrtc_create(&video_id);
            let ws_shared: Rc<RefCell<Option<MediaWs>>> = Rc::new(RefCell::new(None));
            let ws_for_answer = ws_shared.clone();
            let ws_for_local_ice = ws_shared.clone();

            let local_ice_cb = Closure::wrap(Box::new(move |candidate: String, mline: u32| {
                if let Some(ws) = ws_for_local_ice.borrow().as_ref() {
                    let _ = ws.send_ice(candidate, mline);
                }
            }) as Box<dyn FnMut(String, u32)>);
            webrtc_on_ice(pc_handle, &local_ice_cb);
            local_ice_cb.forget();

            let connected_cb = Closure::once(move || {
                connected.set(true);
                status.set("Connected".to_string());
            });
            webrtc_on_connected(pc_handle, &connected_cb);
            connected_cb.forget();

            let ws_result = MediaWs::connect(
                media_id,
                &token,
                move |offer_sdp: String| {
                    let ws_cell = ws_for_answer.clone();
                    let on_answer_cb = Closure::once(move |answer_sdp: String| {
                        if let Some(ws) = ws_cell.borrow().as_ref() {
                            let _ = ws.send_answer(answer_sdp);
                        }
                    });
                    webrtc_set_offer(pc_handle, &offer_sdp, &on_answer_cb);
                    on_answer_cb.forget();
                },
                move |candidate: String, mline_index: u32| {
                    webrtc_add_ice(pc_handle, &candidate, mline_index);
                },
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

    let video_id_fill = video_id.clone();
    let video_id_fullscreen = video_id.clone();
    let video_id_mousemove = video_id.clone();
    let video_id_mousedown = video_id.clone();
    let video_id_mouseup = video_id.clone();
    let video_id_wheel = video_id.clone();
    let container_id_click = container_id.clone();

    // --- Keyboard handlers ---
    let on_keydown = move |evt: KeyboardEvent| {
        evt.prevent_default();
        if evt.is_auto_repeating() {
            return;
        }
        let code = evt.code().to_string();
        if let Some(hid) = browser_code_to_hid(&code) {
            if let Some(ws) = keyboard_ws_keydown.borrow().as_ref() {
                let _ = ws.send_event(KeyboardEventType::KeyDown, hid as u32);
            }
        }
    };

    let on_keyup = move |evt: KeyboardEvent| {
        evt.prevent_default();
        let code = evt.code().to_string();
        if let Some(hid) = browser_code_to_hid(&code) {
            if let Some(ws) = keyboard_ws_keyup.borrow().as_ref() {
                let _ = ws.send_event(KeyboardEventType::KeyUp, hid as u32);
            }
        }
    };

    // --- Mouse handlers ---
    let on_mousemove = move |evt: MouseEvent| {
        if mouse_ws_move.borrow().is_none() {
            return;
        }
        let coords = evt.element_coordinates();
        if let Some((x, y)) = video_coords_to_hid(&video_id_mousemove, coords.x, coords.y) {
            let buttons = build_buttons(evt.held_buttons());
            if let Some(ws) = mouse_ws_move.borrow().as_ref() {
                let _ = ws.send_input(buttons, x, y, 0, 0);
            }
        }
    };

    let on_mousedown = move |evt: MouseEvent| {
        if mouse_ws_down.borrow().is_none() {
            return;
        }
        let coords = evt.element_coordinates();
        if let Some((x, y)) = video_coords_to_hid(&video_id_mousedown, coords.x, coords.y) {
            let buttons = build_buttons(evt.held_buttons());
            if let Some(ws) = mouse_ws_down.borrow().as_ref() {
                let _ = ws.send_input(buttons, x, y, 0, 0);
            }
        }
    };

    let on_mouseup = move |evt: MouseEvent| {
        if mouse_ws_up.borrow().is_none() {
            return;
        }
        let coords = evt.element_coordinates();
        if let Some((x, y)) = video_coords_to_hid(&video_id_mouseup, coords.x, coords.y) {
            let buttons = build_buttons(evt.held_buttons());
            if let Some(ws) = mouse_ws_up.borrow().as_ref() {
                let _ = ws.send_input(buttons, x, y, 0, 0);
            }
        }
    };

    let on_wheel = move |evt: WheelEvent| {
        evt.prevent_default();
        if mouse_ws_wheel.borrow().is_none() {
            return;
        }
        let coords = evt.element_coordinates();
        if let Some((x, y)) = video_coords_to_hid(&video_id_wheel, coords.x, coords.y) {
            let buttons = build_buttons(evt.held_buttons());
            let delta = evt.delta();
            let (wheel, hwheel) = match delta {
                WheelDelta::Pixels(v) => (
                    (-v.y.signum() as i32).clamp(-127, 127),
                    (v.x.signum() as i32).clamp(-127, 127),
                ),
                WheelDelta::Lines(v) => (
                    (-v.y.signum() as i32).clamp(-127, 127),
                    (v.x.signum() as i32).clamp(-127, 127),
                ),
                WheelDelta::Pages(v) => (
                    (-v.y.signum() as i32).clamp(-127, 127),
                    (v.x.signum() as i32).clamp(-127, 127),
                ),
            };
            if let Some(ws) = mouse_ws_wheel.borrow().as_ref() {
                let _ = ws.send_input(buttons, x, y, wheel, hwheel);
            }
        }
    };

    let has_hid = keyboard_id.is_some() || mouse_id.is_some();

    rsx! {
        div {
            id: "{container_id}",
            tabindex: "0",
            style: "outline: none;",
            onkeydown: on_keydown,
            onkeyup: on_keyup,

            // Status bar and controls
            div { style: "display: flex; align-items: center; gap: 0.75rem; margin-bottom: 0.5rem; flex-wrap: wrap;",
                p {
                    style: if connected() {
                        "color: #4caf50; font-size: 0.85rem; margin: 0;"
                    } else {
                        "color: #888; font-size: 0.85rem; margin: 0;"
                    },
                    "{status}"
                }
                for name in led_names().iter() {
                    span {
                        style: "font-size: 0.75rem; padding: 0.15rem 0.4rem; background: #4caf50; border-radius: 3px; color: #fff;",
                        "{name}"
                    }
                }
                button {
                    class: "btn",
                    title: "Fill window",
                    onclick: move |_| webrtc_enter_fill(&video_id_fill),
                    "⤢ Fill window"
                }
                button {
                    class: "btn",
                    title: "Fullscreen",
                    onclick: move |_| request_fullscreen(&video_id_fullscreen),
                    "⛶ Fullscreen"
                }
            }

            if has_hid {
                p { style: "font-size: 0.8rem; color: #888; margin: 0 0 0.4rem 0;",
                    "Click the video to focus keyboard input. Mouse is tracked absolutely within the video."
                }
            }

            video {
                id: "{video_id}",
                autoplay: "true",
                playsinline: "true",
                style: "width: 100%; max-width: 800px; background: #000; border-radius: 4px; display: block; cursor: crosshair;",
                onclick: move |_| {
                    // Focus the container div to receive keyboard events.
                    if let Some(window) = web_sys::window() {
                        if let Some(document) = window.document() {
                            if let Some(el) = document.get_element_by_id(&container_id_click) {
                                if let Some(el) = el.dyn_ref::<web_sys::HtmlElement>() {
                                    let _ = el.focus();
                                }
                            }
                        }
                    }
                },
                onmousemove: on_mousemove,
                onmousedown: on_mousedown,
                onmouseup: on_mouseup,
                onwheel: on_wheel,
            }
        }
    }
}
