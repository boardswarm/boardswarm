use boardswarm_protocol::ItemType;
use dioxus::prelude::*;

#[component]
pub fn DeviceList(token: String, on_device_select: EventHandler<(u64, String)>) -> Element {
    let mut devices = use_signal(Vec::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);

    let token_clone = token.clone();
    use_effect(move || {
        let token = token_clone.clone();
        spawn(async move {
            let mut client = crate::api::create_client(&token);
            let request = boardswarm_protocol::ItemTypeRequest {
                r#type: ItemType::Device.into(),
            };
            match client.list(request).await {
                Ok(resp) => {
                    devices.set(resp.into_inner().item);
                    loading.set(false);
                }
                Err(e) => {
                    error.set(Some(format!("Failed to list devices: {e}")));
                    loading.set(false);
                }
            }
        });
    });

    rsx! {
        h1 { "Devices" }

        if loading() {
            p { "Loading..." }
        }

        if let Some(ref err) = error() {
            p { style: "color: #e94560;", "{err}" }
        }

        if !devices().is_empty() {
            table { class: "item-list",
                thead {
                    tr {
                        th { "ID" }
                        th { "Name" }
                        th { "Instance" }
                        th { "Actions" }
                    }
                }
                tbody {
                    for device in devices().iter() {
                        tr {
                            td { "{device.id}" }
                            td { "{device.name}" }
                            td { "{device.instance.as_deref().unwrap_or(\"-\")}" }
                            td {
                                button {
                                    class: "btn",
                                    onclick: {
                                        let name = device.name.clone();
                                        let id = device.id;
                                        move |_| {
                                            on_device_select.call((id, name.clone()));
                                        }
                                    },
                                    "Details"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
