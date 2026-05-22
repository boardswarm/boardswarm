use boardswarm_protocol::LoginInfo;
use tracing::info;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, Response, window};

/// Authentication state for the web UI.
#[derive(Clone, Debug)]
pub enum AuthState {
    /// Initial state, checking for existing token
    Unknown,
    /// No valid token, need to authenticate
    Unauthenticated { login_info: Vec<LoginInfo> },
    /// Have a valid JWT token
    Authenticated { token: String },
}

/// Check if we have a stored token and if so, return authenticated state.
/// If not, fetch login info from the server.
pub async fn check_auth() -> AuthState {
    // Check sessionStorage for existing token
    if let Some(token) = get_stored_token() {
        return AuthState::Authenticated { token };
    }

    // Check URL for OIDC callback
    if let Some(token) = handle_oidc_callback().await {
        store_token(&token);
        return AuthState::Authenticated { token };
    }

    // No token — fetch login info to show login options
    match fetch_login_info().await {
        Ok(info) => AuthState::Unauthenticated { login_info: info },
        Err(e) => {
            tracing::error!("Failed to fetch login info: {e}");
            AuthState::Unauthenticated { login_info: vec![] }
        }
    }
}

/// Fetch login info from the server (unauthenticated endpoint)
async fn fetch_login_info() -> Result<Vec<LoginInfo>, String> {
    let mut client = crate::api::create_unauth_client();
    let resp = client
        .login_info(())
        .await
        .map_err(|e| format!("gRPC error: {e}"))?;
    Ok(resp.into_inner().info)
}

/// Start OIDC authorization code flow with PKCE.
pub fn start_oidc_login(oidc_url: &str, client_id: &str) {
    let window = window().unwrap();
    let origin = window.location().origin().unwrap();
    let redirect_uri = format!("{origin}/");

    let code_verifier = generate_random_string(64);

    // Store verifier and OIDC metadata for token exchange
    if let Ok(Some(storage)) = window.session_storage() {
        let _ = storage.set_item("pkce_verifier", &code_verifier);
        let _ = storage.set_item("oidc_client_id", client_id);
        let _ = storage.set_item("oidc_url", oidc_url);
    }

    let auth_url = format!(
        "{oidc_url}/authorize?\
         response_type=code\
         &client_id={client_id}\
         &redirect_uri={redirect_uri}\
         &scope=openid\
         &code_challenge={code_verifier}\
         &code_challenge_method=plain\
         &state=boardswarm"
    );

    info!("Redirecting to OIDC: {auth_url}");
    let _ = window.location().set_href(&auth_url);
}

/// Handle OIDC callback (code in URL query params)
async fn handle_oidc_callback() -> Option<String> {
    let window = window()?;
    let search = window.location().search().ok()?;
    if !search.contains("code=") {
        return None;
    }

    let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
    let code = params.get("code")?;

    let storage = window.session_storage().ok()??;
    let verifier = storage.get_item("pkce_verifier").ok()??;
    let client_id = storage.get_item("oidc_client_id").ok()??;
    let oidc_url = storage.get_item("oidc_url").ok()??;

    // Clean up URL (remove query params)
    let _ =
        window
            .history()
            .ok()?
            .replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some("/"));

    // Exchange code for token
    let origin = window.location().origin().ok()?;
    let redirect_uri = format!("{origin}/");
    let token_url = format!("{oidc_url}/token");

    let body = format!(
        "grant_type=authorization_code\
         &code={code}\
         &redirect_uri={redirect_uri}\
         &client_id={client_id}\
         &code_verifier={verifier}"
    );

    let resp = http_post(&token_url, &body).await?;
    let token = parse_token_response(&resp)?;

    // Clean up storage
    let _ = storage.remove_item("pkce_verifier");
    let _ = storage.remove_item("oidc_client_id");
    let _ = storage.remove_item("oidc_url");

    Some(token)
}

fn get_stored_token() -> Option<String> {
    let window = window()?;
    let storage = window.session_storage().ok()??;
    storage.get_item("boardswarm_token").ok()?
}

pub fn store_token(token: &str) {
    if let Some(window) = window() {
        if let Ok(Some(storage)) = window.session_storage() {
            let _ = storage.set_item("boardswarm_token", token);
        }
    }
}

fn generate_random_string(len: usize) -> String {
    use js_sys::Math;
    (0..len)
        .map(|_| {
            let idx = (Math::random() * 36.0) as u8;
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect()
}

async fn http_post(url: &str, body: &str) -> Option<String> {
    let mut opts = RequestInit::new();
    opts.method("POST");
    opts.mode(RequestMode::Cors);
    opts.body(Some(&wasm_bindgen::JsValue::from_str(body)));

    let request = Request::new_with_str_and_init(url, &opts).ok()?;
    request
        .headers()
        .set("Content-Type", "application/x-www-form-urlencoded")
        .ok()?;

    let window = window()?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .ok()?;
    let resp: Response = resp_value.dyn_into().ok()?;
    let text = JsFuture::from(resp.text().ok()?).await.ok()?;
    text.as_string()
}

fn parse_token_response(json_str: &str) -> Option<String> {
    // Simple JSON parsing for access_token field
    let start = json_str.find("\"access_token\"")?;
    let rest = &json_str[start + 15..];
    let start_quote = rest.find('"')? + 1;
    let end_quote = rest[start_quote..].find('"')?;
    Some(rest[start_quote..start_quote + end_quote].to_string())
}
