#![allow(dead_code)]

use dioxus::prelude::*;

mod api;
mod auth;
mod components;
mod ws;

fn main() {
    tracing_wasm::set_as_global_default();
    launch(app);
}

fn app() -> Element {
    let mut auth_state = use_signal(|| auth::AuthState::Unknown);

    // Check auth state on mount
    use_effect(move || {
        spawn(async move {
            let state = auth::check_auth().await;
            auth_state.set(state);
        });
    });

    match auth_state() {
        auth::AuthState::Unknown => rsx! {
            div { class: "login-container",
                p { "Loading..." }
            }
        },
        auth::AuthState::Unauthenticated { login_info } => rsx! {
            components::login::Login { login_info }
        },
        auth::AuthState::Authenticated { token } => rsx! {
            components::layout::AppLayout { token }
        },
    }
}
