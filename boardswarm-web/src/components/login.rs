use boardswarm_protocol::LoginInfo;
use dioxus::prelude::*;

#[component]
pub fn Login(login_info: Vec<LoginInfo>) -> Element {
    let mut token_input = use_signal(String::new);

    rsx! {
        div { class: "login-container",
            h1 { "Boardswarm" }
            p { "Authentication required" }

            // OIDC login buttons
            for info in login_info.iter() {
                if let Some(boardswarm_protocol::login_info::Method::Oidc(ref oidc)) = info.method {
                    button {
                        class: "btn",
                        onclick: {
                            let url = oidc.url.clone();
                            let client_id = oidc.client_id.clone();
                            move |_| {
                                crate::auth::start_oidc_login(&url, &client_id);
                            }
                        },
                        "Login with {info.description}"
                    }
                }
            }

            // Static token input (for development)
            div { style: "margin-top: 2rem; display: flex; flex-direction: column; gap: 0.5rem; align-items: center;",
                p { style: "color: #888; font-size: 0.9rem;", "Or enter a JWT token directly:" }
                input {
                    r#type: "password",
                    placeholder: "JWT token",
                    value: "{token_input}",
                    oninput: move |e| token_input.set(e.value()),
                    style: "padding: 0.4rem; width: 300px; border-radius: 4px; border: 1px solid #444; background: #222; color: #eee;",
                }
                button {
                    class: "btn",
                    onclick: move |_| {
                        let token = token_input().trim().to_string();
                        if !token.is_empty() {
                            crate::auth::store_token(&token);
                            // Reload to pick up the token
                            if let Some(window) = web_sys::window() {
                                let _ = window.location().reload();
                            }
                        }
                    },
                    "Use Token"
                }
            }
        }
    }
}
