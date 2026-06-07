use boardswarm_client::client::{KeyboardSession, MediaSession, MouseSession, SignalMsg};
use gstreamer::prelude::*;
use gstreamer_video::VideoInfo;
use gstreamer_webrtc::{WebRTCSDPType, WebRTCSessionDescription};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Key name → USB HID Usage ID (keyboard page 0x07)
// ---------------------------------------------------------------------------

/// Map a GDK/X11 keysym name (as used by GStreamer navigation events) to a
/// USB HID Usage ID (keyboard page 0x07).  Returns `None` for unknown keys.
pub fn key_name_to_hid(key: &str) -> Option<u8> {
    // Single-character keys
    if key.len() == 1 {
        let c = key.chars().next()?;
        match c {
            'a'..='z' => return Some(0x04 + (c as u8 - b'a')),
            'A'..='Z' => return Some(0x04 + (c as u8 - b'A')),
            '1'..='9' => return Some(0x1E + (c as u8 - b'1')),
            '0' => return Some(0x27),
            ' ' => return Some(0x2C),
            _ => {}
        }
    }

    match key {
        "Return" | "KP_Enter" => Some(0x28),
        "Escape" => Some(0x29),
        "BackSpace" => Some(0x2A),
        "Tab" | "ISO_Left_Tab" => Some(0x2B),
        "space" => Some(0x2C),
        "minus" | "underscore" => Some(0x2D),
        "equal" | "plus" => Some(0x2E),
        "bracketleft" | "braceleft" => Some(0x2F),
        "bracketright" | "braceright" => Some(0x30),
        "backslash" | "bar" => Some(0x31),
        "numbersign" | "asciitilde" | "semicolon" | "colon" => Some(0x33),
        "apostrophe" | "quotedbl" => Some(0x34),
        "grave" => Some(0x35),
        "comma" | "less" => Some(0x36),
        "period" | "greater" => Some(0x37),
        "slash" | "question" => Some(0x38),
        "Caps_Lock" => Some(0x39),
        "F1" => Some(0x3A),
        "F2" => Some(0x3B),
        "F3" => Some(0x3C),
        "F4" => Some(0x3D),
        "F5" => Some(0x3E),
        "F6" => Some(0x3F),
        "F7" => Some(0x40),
        "F8" => Some(0x41),
        "F9" => Some(0x42),
        "F10" => Some(0x43),
        "F11" => Some(0x44),
        "F12" => Some(0x45),
        "Print" | "Sys_Req" => Some(0x46),
        "Scroll_Lock" => Some(0x47),
        "Pause" | "Break" => Some(0x48),
        "Insert" => Some(0x49),
        "Home" => Some(0x4A),
        "Prior" | "Page_Up" => Some(0x4B),
        "Delete" => Some(0x4C),
        "End" => Some(0x4D),
        "Next" | "Page_Down" => Some(0x4E),
        "Right" => Some(0x4F),
        "Left" => Some(0x50),
        "Down" => Some(0x51),
        "Up" => Some(0x52),
        "Num_Lock" => Some(0x53),
        "KP_Divide" => Some(0x54),
        "KP_Multiply" => Some(0x55),
        "KP_Subtract" => Some(0x56),
        "KP_Add" => Some(0x57),
        "KP_0" => Some(0x62),
        "KP_1" => Some(0x59),
        "KP_2" => Some(0x5A),
        "KP_3" => Some(0x5B),
        "KP_4" => Some(0x5C),
        "KP_5" => Some(0x5D),
        "KP_6" => Some(0x5E),
        "KP_7" => Some(0x5F),
        "KP_8" => Some(0x60),
        "KP_9" => Some(0x61),
        "KP_Decimal" | "KP_Separator" => Some(0x63),
        "Control_L" => Some(0xE0),
        "Shift_L" => Some(0xE1),
        "Alt_L" => Some(0xE2),
        "Super_L" | "Meta_L" => Some(0xE3),
        "Control_R" => Some(0xE4),
        "Shift_R" => Some(0xE5),
        "Alt_R" => Some(0xE6),
        "Super_R" | "Meta_R" => Some(0xE7),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Internal event types bridged from the blocking bus thread → async tasks
// ---------------------------------------------------------------------------

enum HidKeyEvent {
    Down(u8),
    Up(u8),
}

#[derive(Debug)]
struct HidMouseEvent {
    buttons: u8,
    x: i16,
    y: i16,
    wheel: i8,
    hwheel: i8,
}

enum OutboundSignal {
    Answer(String),
    Ice { candidate: String, mline_index: u32 },
}

// ---------------------------------------------------------------------------
// Pipeline builder (refactored from main.rs)
// ---------------------------------------------------------------------------

pub fn build_receive_pipeline(stun_server: &str) -> (gstreamer::Pipeline, gstreamer::Element) {
    let pipeline = gstreamer::Pipeline::new();

    let webrtcbin = gstreamer::ElementFactory::make("webrtcbin")
        .name("recvbin")
        .property_from_str("bundle-policy", "max-bundle")
        .property_from_str("latency", "16")
        .build()
        .expect("webrtcbin — install gstreamer1.0-plugins-bad");

    if !stun_server.is_empty() {
        webrtcbin.set_property("stun-server", stun_server);
    }

    pipeline.add(&webrtcbin).unwrap();

    let pipeline_weak = pipeline.downgrade();
    webrtcbin.connect_pad_added(move |_webrtcbin, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        if pad.direction() != gstreamer::PadDirection::Src {
            return;
        }

        let queue = gstreamer::ElementFactory::make("queue")
            .build()
            .expect("queue");
        let depay = gstreamer::ElementFactory::make("rtph264depay")
            .build()
            .expect("rtph264depay — install gstreamer1.0-plugins-good");
        let parse = gstreamer::ElementFactory::make("h264parse")
            .build()
            .expect("h264parse — install gstreamer1.0-plugins-bad");
        let decodebin = gstreamer::ElementFactory::make("decodebin")
            .build()
            .expect("decodebin");
        let convert = gstreamer::ElementFactory::make("videoconvert")
            .build()
            .expect("videoconvert");
        let sink = gstreamer::ElementFactory::make("autovideosink")
            .build()
            .expect("autovideosink — install gstreamer1.0-plugins-good");
        sink.set_property("sync", false);

        pipeline
            .add_many([&queue, &depay, &parse, &decodebin, &convert, &sink])
            .unwrap();
        gstreamer::Element::link_many([&queue, &depay, &parse, &decodebin]).unwrap();

        let convert_weak = convert.downgrade();
        let sink_weak = sink.downgrade();
        decodebin.connect_pad_added(move |_, src_pad| {
            let Some(convert) = convert_weak.upgrade() else {
                return;
            };
            let Some(sink) = sink_weak.upgrade() else {
                return;
            };
            let caps = src_pad
                .current_caps()
                .unwrap_or_else(|| src_pad.query_caps(None));
            if caps.iter().any(|s| s.name().starts_with("video/x-raw")) {
                let convert_sink = convert.static_pad("sink").unwrap();
                if !convert_sink.is_linked() {
                    src_pad
                        .link(&convert_sink)
                        .expect("decodebin → videoconvert");
                    gstreamer::Element::link(&convert, &sink)
                        .expect("videoconvert → autovideosink");
                    convert.sync_state_with_parent().unwrap();
                    sink.sync_state_with_parent().unwrap();
                }
            }
        });

        let queue_sink = queue.static_pad("sink").unwrap();
        pad.link(&queue_sink).expect("webrtcbin → queue");
        for el in [&queue, &depay, &parse, &decodebin] {
            el.sync_state_with_parent().unwrap();
        }
    });

    (pipeline, webrtcbin)
}

// ---------------------------------------------------------------------------
// Navigation event parsing helpers
// ---------------------------------------------------------------------------

/// Try to parse a navigation event from a GstMessage posted on the bus by a
/// video sink.  Navigation events are posted as `GST_MESSAGE_ELEMENT` messages
/// with the structure name `"GstNavigationMessage"`.  The structure contains a
/// field `"event"` that is a `GstEvent` carrying the actual navigation data.
///
/// We parse the event structure directly since the gstreamer-video crate's
/// `NavigationEvent` API may not be available in all version configurations.
fn parse_navigation_from_message(msg: &gstreamer::Message) -> Option<NavEvent> {
    let gstreamer::MessageView::Element(elem) = msg.view() else {
        return None;
    };
    let structure = elem.structure()?;
    if structure.name() != "GstNavigationMessage" {
        return None;
    }
    let event: gstreamer::Event = structure.get("event").ok()?;
    let src = msg.src()?;
    let video_sink: gstreamer::Element = src.clone().downcast().ok()?;
    let (width, height) =
        if let Some(caps) = video_sink.static_pad("sink").and_then(|p| p.current_caps()) {
            let vid = VideoInfo::from_caps(&caps).ok()?;
            (vid.width(), vid.height())
        } else {
            (1920, 1080)
        };

    let nav_structure = event.structure()?;
    parse_navigation_structure(width, height, nav_structure)
}

enum NavEvent {
    KeyPress(String),
    KeyRelease(String),
    MouseMove {
        x: i16,
        y: i16,
    },
    MouseButtonPress {
        button: u8,
        x: i16,
        y: i16,
    },
    MouseButtonRelease {
        button: u8,
        x: i16,
        y: i16,
    },
    MouseScroll {
        x: i16,
        y: i16,
        delta_x: i8,
        delta_y: i8,
    },
}

fn scale_axis(v: f64, max: u32) -> i16 {
    let maxf = max as f64;
    let v = v.clamp(0.0, maxf);
    (v / maxf * i16::MAX as f64) as i16
}

fn scale_wheel(v: f64, max: u32) -> i8 {
    let maxf = max as f64;
    let v = v.clamp(0.0, maxf);
    (v / maxf * i8::MAX as f64) as i8
}

fn get_pos(width: u32, height: u32, s: &gstreamer::StructureRef) -> Option<(i16, i16)> {
    let x: f64 = s.get("pointer_x").ok()?;
    let y: f64 = s.get("pointer_y").ok()?;
    Some((scale_axis(x, width), scale_axis(y, height)))
}

fn parse_navigation_structure(
    width: u32,
    height: u32,
    s: &gstreamer::StructureRef,
) -> Option<NavEvent> {
    let event_type: String = s.get("event").ok()?;
    match event_type.as_str() {
        "key-press" => {
            let key: String = s.get("key").ok()?;
            Some(NavEvent::KeyPress(key))
        }
        "key-release" => {
            let key: String = s.get("key").ok()?;
            Some(NavEvent::KeyRelease(key))
        }
        "mouse-move" => {
            let (x, y) = get_pos(width, height, s)?;
            Some(NavEvent::MouseMove { x, y })
        }
        "mouse-button-press" => {
            let button = s.get::<i32>("button").ok()? as u8;
            let (x, y) = get_pos(width, height, s)?;

            match button {
                1 => Some(NavEvent::MouseButtonPress { button: 0, x, y }),
                3 => Some(NavEvent::MouseButtonPress { button: 1, x, y }),
                2 => Some(NavEvent::MouseButtonPress { button: 2, x, y }),
                8 => Some(NavEvent::MouseButtonPress { button: 3, x, y }),
                9 => Some(NavEvent::MouseButtonPress { button: 4, x, y }),
                4 => Some(NavEvent::MouseScroll {
                    x,
                    y,
                    delta_x: 0,
                    delta_y: 1,
                }),
                5 => Some(NavEvent::MouseScroll {
                    x,
                    y,
                    delta_x: 0,
                    delta_y: -1,
                }),
                6 => Some(NavEvent::MouseScroll {
                    x,
                    y,
                    delta_x: 1,
                    delta_y: 0,
                }),
                7 => Some(NavEvent::MouseScroll {
                    x,
                    y,
                    delta_x: -1,
                    delta_y: 0,
                }),

                _ => None,
            }
        }
        "mouse-button-release" => {
            let button = s.get::<i32>("button").ok()? as u8;
            let (x, y) = get_pos(width, height, s)?;
            match button {
                1 => Some(NavEvent::MouseButtonRelease { button: 0, x, y }),
                3 => Some(NavEvent::MouseButtonRelease { button: 1, x, y }),
                2 => Some(NavEvent::MouseButtonRelease { button: 2, x, y }),
                8 => Some(NavEvent::MouseButtonRelease { button: 3, x, y }),
                9 => Some(NavEvent::MouseButtonRelease { button: 4, x, y }),
                _ => None,
            }
        }
        "mouse-scroll" => {
            let (x, y) = get_pos(width, height, s)?;
            let delta_x: f64 = s.get("delta_pointer_x").unwrap_or(0.0);
            let delta_x = scale_wheel(delta_x, width);
            let delta_y: f64 = s.get("delta_pointer_y").unwrap_or(0.0);
            let delta_y = scale_wheel(delta_y, height);
            Some(NavEvent::MouseScroll {
                x,
                y,
                delta_x,
                delta_y,
            })
        }
        _ => None,
    }
}

/// Map GStreamer button number, based on x11 (1=left, 2=middle, 3=right) to bitmask bit.
fn set_mouse_button(buttons: &mut u8, button: u8, pressed: bool) {
    if pressed {
        *buttons |= 1 << button;
    } else {
        *buttons &= !(1 << button);
    }
}

// ---------------------------------------------------------------------------
// Main KVM function
// ---------------------------------------------------------------------------

pub async fn run_kvm(
    mut keyboard: Option<KeyboardSession>,
    mut mouse: Option<MouseSession>,
    mut media: MediaSession,
) -> anyhow::Result<()> {
    gstreamer::init()?;

    let stun_server = "stun://stun.l.google.com:19302";
    let (pipeline, webrtcbin) = build_receive_pipeline(stun_server);

    let (outbound_tx, mut outbound_rx) = tokio_mpsc::unbounded_channel::<OutboundSignal>();
    let (key_tx, mut key_rx) = tokio_mpsc::unbounded_channel::<HidKeyEvent>();
    let (mouse_tx, mut mouse_rx) = tokio_mpsc::unbounded_channel::<HidMouseEvent>();

    let tx_ice = outbound_tx.clone();
    webrtcbin.connect("on-ice-candidate", false, move |values| {
        let mline_index = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        let _ = tx_ice.send(OutboundSignal::Ice {
            candidate,
            mline_index,
        });
        None
    });

    pipeline.set_state(gstreamer::State::Playing)?;
    info!("GStreamer KVM pipeline started");

    let bus = pipeline.bus().expect("Pipeline has no bus");
    let pipeline_for_bus = pipeline.clone();
    let key_tx_bus = key_tx;
    let mouse_tx_bus = mouse_tx;

    std::thread::spawn(move || {
        let mut mouse_buttons: u8 = 0;

        for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
            match msg.view() {
                gstreamer::MessageView::Eos(..) => {
                    warn!("KVM stream ended (EOS)");
                    break;
                }
                gstreamer::MessageView::Error(err) => {
                    tracing::error!(
                        "Pipeline error from {:?}: {}",
                        err.src().map(|s| s.path_string()),
                        err.error()
                    );
                    break;
                }
                _ => {
                    if let Some(nav) = parse_navigation_from_message(&msg) {
                        handle_nav_event(nav, &key_tx_bus, &mouse_tx_bus, &mut mouse_buttons);
                    }
                }
            }
        }
        let _ = pipeline_for_bus.set_state(gstreamer::State::Null);
    });

    println!("KVM session established — waiting for stream…");

    // Async task: keyboard events → KeyboardSession
    tokio::spawn(async move {
        while let Some(ev) = key_rx.recv().await {
            if let Some(ref mut ks) = keyboard {
                let result = match ev {
                    HidKeyEvent::Down(id) => ks.send_key_down(id).await,
                    HidKeyEvent::Up(id) => ks.send_key_up(id).await,
                };
                if let Err(e) = result {
                    warn!("Failed to send keyboard event: {e}");
                    break;
                }
            }
        }
    });

    // Async task: mouse events → MouseSession
    tokio::spawn(async move {
        while let Some(ev) = mouse_rx.recv().await {
            if let Some(ref mut ms) = mouse {
                if let Err(e) = ms
                    .send_input(ev.buttons, ev.x, ev.y, ev.wheel, ev.hwheel)
                    .await
                {
                    warn!("Failed to send mouse event: {e}");
                    break;
                }
            }
        }
    });

    // Main loop: WebRTC signaling exchange
    loop {
        tokio::select! {
            Some(outbound) = outbound_rx.recv() => {
                match outbound {
                    OutboundSignal::Answer(sdp) => { media.send_answer(sdp).await?; }
                    OutboundSignal::Ice { candidate, mline_index } => {
                        media.send_ice(candidate, mline_index).await?;
                    }
                }
            }
            msg = media.next_signal() => {
                match msg {
                    Some(Ok(SignalMsg::Offer(sdp))) => {
                        info!("Received offer, creating answer…");
                        let sdp_msg = gstreamer_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                            .map_err(|e| anyhow::anyhow!("SDP parse error: {e}"))?;
                        let remote_desc = WebRTCSessionDescription::new(
                            WebRTCSDPType::Offer,
                            sdp_msg,
                        );
                        webrtcbin.emit_by_name::<()>(
                            "set-remote-description",
                            &[&remote_desc, &None::<gstreamer::Promise>],
                        );
                        let webrtcbin_promise = webrtcbin.clone();
                        let tx_answer = outbound_tx.clone();
                        let promise = gstreamer::Promise::with_change_func(move |reply| {
                            let Ok(Some(reply)) = reply else { return; };
                            let answer = reply
                                .get::<WebRTCSessionDescription>("answer")
                                .expect("answer in promise");
                            webrtcbin_promise.emit_by_name::<()>(
                                "set-local-description",
                                &[&answer, &None::<gstreamer::Promise>],
                            );
                            let sdp_text = answer.sdp().as_text().unwrap();
                            let _ = tx_answer.send(OutboundSignal::Answer(sdp_text));
                        });
                        webrtcbin.emit_by_name::<()>(
                            "create-answer",
                            &[&None::<gstreamer::Structure>, &promise],
                        );
                    }
                    Some(Ok(SignalMsg::Ice { candidate, mline_index })) => {
                        webrtcbin.emit_by_name::<()>(
                            "add-ice-candidate",
                            &[&mline_index, &candidate],
                        );
                    }
                    Some(Ok(SignalMsg::Answer(_))) => {
                        warn!("Unexpected answer from server (server is the offerer)");
                    }
                    Some(Err(e)) => return Err(e.into()),
                    None => {
                        info!("KVM media session ended by server");
                        break;
                    }
                }
            }
        }
    }

    pipeline.set_state(gstreamer::State::Null)?;
    info!("KVM shut down cleanly");
    Ok(())
}

// ---------------------------------------------------------------------------
// Navigation event dispatch
// ---------------------------------------------------------------------------

fn handle_nav_event(
    event: NavEvent,
    key_tx: &tokio_mpsc::UnboundedSender<HidKeyEvent>,
    mouse_tx: &tokio_mpsc::UnboundedSender<HidMouseEvent>,
    buttons: &mut u8,
) {
    match event {
        NavEvent::KeyPress(key) => {
            if let Some(id) = key_name_to_hid(&key) {
                let _ = key_tx.send(HidKeyEvent::Down(id));
            }
        }
        NavEvent::KeyRelease(key) => {
            if let Some(id) = key_name_to_hid(&key) {
                let _ = key_tx.send(HidKeyEvent::Up(id));
            }
        }
        NavEvent::MouseMove { x, y } => {
            let _ = mouse_tx.send(HidMouseEvent {
                buttons: *buttons,
                x,
                y,
                wheel: 0,
                hwheel: 0,
            });
        }
        NavEvent::MouseButtonPress { button, x, y } => {
            set_mouse_button(buttons, button, true);
            let event = HidMouseEvent {
                buttons: *buttons,
                x,
                y,
                wheel: 0,
                hwheel: 0,
            };
            eprintln!("Press: {event:#?}");
            let _ = mouse_tx.send(event);
        }
        NavEvent::MouseButtonRelease { button, x, y } => {
            set_mouse_button(buttons, button, false);
            let _ = mouse_tx.send(HidMouseEvent {
                buttons: *buttons,
                x,
                y,
                wheel: 0,
                hwheel: 0,
            });
        }
        NavEvent::MouseScroll {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            let _ = mouse_tx.send(HidMouseEvent {
                buttons: *buttons,
                x,
                y,
                wheel: delta_y,
                hwheel: delta_x,
            });
        }
    }
}
