use boardswarm_protocol::{DeviceRequest, ItemType};
use dioxus::prelude::*;

#[component]
pub fn DeviceDetail(
    device_id: u64,
    device_name: String,
    token: String,
    on_console_open: EventHandler<(u64, String)>,
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
            div {
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
        } else if error().is_none() {
            p { "Loading device info..." }
        }
    }
}
