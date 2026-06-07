use dioxus::prelude::*;

use crate::components::console::ConsoleView;
use crate::components::device_detail::DeviceDetail;
use crate::components::device_list::DeviceList;
use crate::components::kvm::KvmViewer;
use crate::components::media::MediaViewer;

#[derive(Clone, Debug, PartialEq)]
enum Page {
    Devices,
    DeviceDetail { id: u64, name: String },
    Console { id: u64, name: String },
    Media { id: u64, name: String },
    Kvm { media_id: u64, keyboard_id: Option<u64>, mouse_id: Option<u64>, name: String },
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
                hr { style: "border-color: #333; margin: 1rem 0;" }
                a {
                    onclick: move |_| {
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
                            on_device_select: move |(id, name): (u64, String)| {
                                current_page.set(Page::DeviceDetail { id, name });
                            },
                        }
                    },
                    Page::DeviceDetail { id, ref name } => rsx! {
                        a {
                            onclick: move |_| current_page.set(Page::Devices),
                            style: "color: #e94560; cursor: pointer; margin-bottom: 1rem; display: inline-block;",
                            "← Back to devices"
                        }
                        DeviceDetail {
                            device_id: id,
                            device_name: name.clone(),
                            token: token.clone(),
                            on_console_open: move |(id, name): (u64, String)| {
                                current_page.set(Page::Console { id, name });
                            },
                            on_media_open: move |(id, name): (u64, String)| {
                                current_page.set(Page::Media { id, name });
                            },
                            on_kvm_open: move |(media_id, keyboard_id, mouse_id, name): (u64, Option<u64>, Option<u64>, String)| {
                                current_page.set(Page::Kvm { media_id, keyboard_id, mouse_id, name });
                            },
                        }
                    },
                    Page::Console { id, ref name } => rsx! {
                        a {
                            onclick: move |_| current_page.set(Page::Devices),
                            style: "color: #e94560; cursor: pointer; margin-bottom: 1rem; display: inline-block;",
                            "← Back to devices"
                        }
                        h2 { "Console: {name}" }
                        ConsoleView { console_id: id, token: token.clone() }
                    },
                    Page::Media { id, ref name } => rsx! {
                        a {
                            onclick: move |_| current_page.set(Page::Devices),
                            style: "color: #e94560; cursor: pointer; margin-bottom: 1rem; display: inline-block;",
                            "← Back to devices"
                        }
                        h2 { "Media: {name}" }
                        MediaViewer { media_id: id, token: token.clone() }
                    },
                    Page::Kvm { media_id, keyboard_id, mouse_id, ref name } => rsx! {
                        a {
                            onclick: move |_| current_page.set(Page::Devices),
                            style: "color: #e94560; cursor: pointer; margin-bottom: 1rem; display: inline-block;",
                            "← Back to devices"
                        }
                        h2 { "KVM: {name}" }
                        KvmViewer {
                            media_id,
                            keyboard_id,
                            mouse_id,
                            token: token.clone(),
                        }
                    },
                }
            }
        }
    }
}
