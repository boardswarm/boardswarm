use boardswarm_protocol::boardswarm_client::BoardswarmClient;
use tonic_web_wasm_client::Client;

/// Create a gRPC-Web client connected to the boardswarm server.
/// The base URL is derived from the current page origin.
pub fn create_client(token: &str) -> BoardswarmClient<Client> {
    let base_url = web_sys::window()
        .unwrap()
        .location()
        .origin()
        .unwrap_or_else(|_| "http://localhost:6683".to_string());

    let mut client = Client::new(base_url);
    client.set_header("authorization", &format!("Bearer {token}"));
    BoardswarmClient::new(client)
}

/// Create an unauthenticated client (for LoginInfo)
pub fn create_unauth_client() -> BoardswarmClient<Client> {
    let base_url = web_sys::window()
        .unwrap()
        .location()
        .origin()
        .unwrap_or_else(|_| "http://localhost:6683".to_string());

    BoardswarmClient::new(Client::new(base_url))
}
