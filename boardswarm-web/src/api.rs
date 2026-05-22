use boardswarm_protocol::boardswarm_client::BoardswarmClient;
use tonic::service::interceptor::InterceptedService;
use tonic_web_wasm_client::Client;

pub(crate) type AuthClient = BoardswarmClient<InterceptedService<Client, AuthInterceptor>>;

#[derive(Clone)]
pub(crate) struct AuthInterceptor {
    token: String,
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let value = format!("Bearer {}", self.token)
            .parse()
            .map_err(|_| tonic::Status::internal("invalid token"))?;
        req.metadata_mut().insert("authorization", value);
        Ok(req)
    }
}

/// Create a gRPC-Web client with JWT authentication.
pub fn create_client(token: &str) -> AuthClient {
    let base_url = web_sys::window()
        .unwrap()
        .location()
        .origin()
        .unwrap_or_else(|_| "http://localhost:6683".to_string());

    let client = Client::new(base_url);
    let interceptor = AuthInterceptor {
        token: token.to_string(),
    };
    BoardswarmClient::with_interceptor(client, interceptor)
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
