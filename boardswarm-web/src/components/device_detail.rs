use boardswarm_protocol::DeviceRequest;
use dioxus::prelude::*;

#[component]
pub fn DeviceDetail(
    device_id: u64,
    device_name: String,
    token: String,
    on_console_open: EventHandler<(u64, String)>,
    on_media_open: EventHandler<(u64, String)>,
    on_kvm_open: EventHandler<(u64, Option<u64>, Option<u64>, String)>,
) -> Element {
    let mut device_info = use_signal(|| None::<boardswarm_protocol::Device>);
    let mut error = use_signal(|| None::<String>);

    let token_clone = token.clone();
    use_effect(move || {
        let token = token_clone.clone();
        spawn(async move {
            let mut client = crate::api::create_client(&token);
            let request = DeviceRequest { device: device_id };
            match client.device_info(request).await {
                Ok(resp) => {
                    let mut stream = resp.into_inner();
                    // Get the first update
                    if let Ok(Some(info)) = stream.message().await {
                        device_info.set(Some(info));
                    }
                }
                Err(e) => {
                    error.set(Some(format!("Failed to get device info: {e}")));
                }
            }
        });
    });

    rsx! {
        h1 { "{device_name}" }

        if let Some(ref err) = error() {
            p { style: "color: #e94560;", "{err}" }
        }

        if let Some(ref info) = device_info() {
            // Current mode
            div { style: "margin-bottom: 1rem;",
                strong { "Current mode: " }
                span {
                    "{info.current_mode.as_deref().unwrap_or(\"none\")}"
                }
            }

            // Modes
            div { style: "margin-bottom: 1.5rem;",
                h3 { "Modes" }
                div { style: "display: flex; gap: 0.5rem; flex-wrap: wrap;",
                    for mode in info.modes.iter() {
                        {
                            let mode_name = mode.name.clone();
                            let is_current = info.current_mode.as_deref() == Some(&mode.name);
                            let is_available = mode.available;
                            let token = token.clone();
                            rsx! {
                                button {
                                    class: "btn",
                                    disabled: is_current || !is_available,
                                    style: if is_current { "opacity: 0.5;" } else { "" },
                                    onclick: move |_| {
                                        let token = token.clone();
                                        let mode_name = mode_name.clone();
                                        spawn(async move {
                                            let mut client = crate::api::create_client(&token);
                                            let request = boardswarm_protocol::DeviceModeRequest {
                                                device: device_id,
                                                mode: mode_name,
                                            };
                                            if let Err(e) = client.device_change_mode(request).await {
                                                tracing::error!("Mode change failed: {e}");
                                            }
                                        });
                                    },
                                    "{mode.name}"
                                    if !is_available {
                                        " (unavailable)"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Consoles
            div { style: "margin-bottom: 1.5rem;",
                h3 { "Consoles" }
                if info.consoles.is_empty() {
                    p { style: "color: #888;", "No consoles available" }
                }
                for console in info.consoles.iter() {
                    div { style: "display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.25rem;",
                        span { "{console.name}" }
                        if let Some(id) = console.id {
                            button {
                                class: "btn",
                                onclick: {
                                    let name = console.name.clone();
                                    move |_| {
                                        on_console_open.call((id, name.clone()));
                                    }
                                },
                                "Open"
                            }
                        } else {
                            span { style: "color: #888;", "(offline)" }
                        }
                    }
                }
            }

            // Volumes
            div { style: "margin-bottom: 1.5rem;",
                h3 { "Volumes" }
                if info.volumes.is_empty() {
                    p { style: "color: #888;", "No volumes available" }
                }
                for volume in info.volumes.iter() {
                    div { style: "margin-bottom: 0.25rem;",
                        span { "{volume.name}" }
                        if volume.id.is_none() {
                            span { style: "color: #888;", " (offline)" }
                        }
                    }
                }
            }

            // Media
            div { style: "margin-bottom: 1.5rem;",
                h3 { "Media" }
                if info.media.is_empty() {
                    p { style: "color: #888;", "No media available" }
                }
                for item in info.media.iter() {
                    div { style: "display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.25rem;",
                        span { "{item.name}" }
                        if let Some(media_id) = item.id {
                            {
                                let name = item.name.clone();
                                // First keyboard and mouse with an id (i.e. available)
                                let keyboard_id = info.keyboards.iter().find_map(|k| k.id);
                                let mouse_id = info.mice.iter().find_map(|m| m.id);
                                let name_for_kvm = name.clone();
                                rsx! {
                                    // KVM: video + keyboard + mouse (if both HID devices available)
                                    if keyboard_id.is_some() || mouse_id.is_some() {
                                        button {
                                            class: "btn",
                                            onclick: move |_| {
                                                on_kvm_open.call((media_id, keyboard_id, mouse_id, name_for_kvm.clone()));
                                            },
                                            "KVM"
                                        }
                                    }
                                    // View: video-only stream
                                    button {
                                        class: "btn",
                                        onclick: {
                                            let name = name.clone();
                                            move |_| {
                                                on_media_open.call((media_id, name.clone()));
                                            }
                                        },
                                        "View"
                                    }
                                }
                            }
                        } else {
                            span { style: "color: #888;", "(offline)" }
                        }
                    }
                }
            }

            // Keyboards
            if !info.keyboards.is_empty() {
                div { style: "margin-bottom: 1.5rem;",
                    h3 { "Keyboards" }
                    for kb in info.keyboards.iter() {
                        div { style: "margin-bottom: 0.25rem;",
                            span { "{kb.name}" }
                            if kb.id.is_none() {
                                span { style: "color: #888;", " (offline)" }
                            }
                        }
                    }
                }
            }

            // Mice
            if !info.mice.is_empty() {
                div { style: "margin-bottom: 1.5rem;",
                    h3 { "Mice" }
                    for ms in info.mice.iter() {
                        div { style: "margin-bottom: 0.25rem;",
                            span { "{ms.name}" }
                            if ms.id.is_none() {
                                span { style: "color: #888;", " (offline)" }
                            }
                        }
                    }
                }
            }
        } else if error().is_none() {
            p { "Loading device info..." }
        }
    }
}
