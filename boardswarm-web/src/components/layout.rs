use dioxus::prelude::*;

use crate::components::console::ConsoleView;
use crate::components::device_list::DeviceList;

#[derive(Clone, Debug, PartialEq)]
enum Page {
    Devices,
    Consoles,
    Actuators,
    Volumes,
    Console { id: u64, name: String },
}

#[component]
pub fn AppLayout(token: String) -> Element {
    let mut current_page = use_signal(|| Page::Devices);

    rsx! {
        div { class: "app-container",
            // Sidebar
            nav { class: "sidebar",
                h2 { "Boardswarm" }
                a {
                    class: if matches!(current_page(), Page::Devices) { "active" } else { "" },
                    onclick: move |_| current_page.set(Page::Devices),
                    "Devices"
                }
                a {
                    class: if matches!(current_page(), Page::Consoles) { "active" } else { "" },
                    onclick: move |_| current_page.set(Page::Consoles),
                    "Consoles"
                }
                a {
                    class: if matches!(current_page(), Page::Actuators) { "active" } else { "" },
                    onclick: move |_| current_page.set(Page::Actuators),
                    "Actuators"
                }
                a {
                    class: if matches!(current_page(), Page::Volumes) { "active" } else { "" },
                    onclick: move |_| current_page.set(Page::Volumes),
                    "Volumes"
                }
                hr { style: "border-color: #333; margin: 1rem 0;" }
                a {
                    onclick: move |_| {
                        // Clear token and reload
                        if let Some(window) = web_sys::window() {
                            if let Ok(Some(storage)) = window.session_storage() {
                                let _ = storage.remove_item("boardswarm_token");
                            }
                            let _ = window.location().reload();
                        }
                    },
                    "Logout"
                }
            }

            // Main content
            div { class: "main-content",
                match current_page() {
                    Page::Devices => rsx! {
                        DeviceList {
                            token: token.clone(),
                            on_console_open: move |(id, name): (u64, String)| {
                                current_page.set(Page::Console { id, name });
                            },
                        }
                    },
                    Page::Consoles => rsx! {
                        h1 { "Consoles" }
                        DeviceList {
                            token: token.clone(),
                            on_console_open: move |(id, name): (u64, String)| {
                                current_page.set(Page::Console { id, name });
                            },
                        }
                    },
                    Page::Actuators => rsx! {
                        h1 { "Actuators" }
                        p { "Coming soon" }
                    },
                    Page::Volumes => rsx! {
                        h1 { "Volumes" }
                        p { "Coming soon" }
                    },
                    Page::Console { id, ref name } => rsx! {
                        div {
                            h2 { "Console: {name}" }
                            ConsoleView { console_id: id, token: token.clone() }
                        }
                    },
                }
            }
        }
    }
}
