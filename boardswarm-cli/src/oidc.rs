use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use oauth2::basic::BasicClient;
use oauth2::basic::BasicTokenResponse;
use oauth2::devicecode::StandardDeviceAuthorizationResponse;
use oauth2::reqwest::async_http_client;
use oauth2::DeviceCodeErrorResponseType;
use oauth2::RequestTokenError;
use oauth2::StandardErrorResponse;

use oauth2::RefreshToken;
use oauth2::{AuthUrl, ClientId, DeviceAuthorizationUrl, Scope, TokenResponse, TokenUrl};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tracing::debug;
use tracing::info;
use tracing::warn;

#[derive(Deserialize, Clone, Debug)]
pub struct OidcDiscovery {
    pub authorization_endpoint: String,
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
}

impl OidcDiscovery {
    async fn from_uri(mut url: Url) -> Result<OidcDiscovery> {
        url.path_segments_mut()
            .map_err(|_| anyhow!("Invalid url"))?
            .pop_if_empty()
            .extend(&[".well-known", "openid-configuration"]);
        Client::new()
            .get(url)
            .send()
            .await?
            .json()
            .await
            .map_err(Into::into)
    }
}

fn since_epoch_se<S>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(t) = t {
        let epoch = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        s.serialize_u64(epoch)
    } else {
        s.serialize_none()
    }
}

fn since_epoch_de<'de, D>(de: D) -> Result<Option<SystemTime>, D::Error>
where
    D: Deserializer<'de>,
{
    let since = u64::deserialize(de)?;
    Ok(Some(UNIX_EPOCH + Duration::from_secs(since)))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Token {
    refresh: Option<RefreshToken>,
    #[serde(serialize_with = "since_epoch_se", deserialize_with = "since_epoch_de")]
    expires: Option<SystemTime>,
    access: String,
}

impl Token {
    pub async fn from_file<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let mut f = tokio::fs::File::open(path).await?;
        let mut data = Vec::new();
        f.read_to_end(&mut data).await?;
        let t = serde_yaml::from_slice(&data)?;
        Ok(t)
    }

    pub async fn to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let mut f = tokio::fs::File::options()
            .write(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .await?;
        let s = serde_yaml::to_string(self)?;
        f.write_all(s.as_bytes()).await?;
        f.flush().await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("Login aborted")]
    Aborted,
    #[error("Response error: {0}")]
    Response(
        RequestTokenError<
            oauth2::reqwest::Error<reqwest::Error>,
            StandardErrorResponse<DeviceCodeErrorResponseType>,
        >,
    ),
}

type LoginResult = Result<BasicTokenResponse, LoginError>;

#[derive(Debug)]
pub struct LoginRequest {
    client: BasicClient,
    details: StandardDeviceAuthorizationResponse,
}

impl LoginRequest {
    pub async fn get_token(&self) -> LoginResult {
        let token = self
            .client
            .exchange_device_access_token(&self.details)
            .request_async(async_http_client, tokio::time::sleep, None)
            .await
            .map_err(LoginError::Response)?;
        Ok(token)
    }

    fn verification_uri(&self) -> &Url {
        self.details.verification_uri().url()
    }

    fn user_code(&self) -> &str {
        self.details.user_code().secret()
    }

    fn verification_uri_complete(&self) -> Option<&str> {
        self.details
            .verification_uri_complete()
            .map(|u| u.secret().as_str())
    }
}

#[async_trait::async_trait]
pub trait LoginProvider: std::fmt::Debug + Send + Sync {
    async fn login(&self, login_request: LoginRequest) -> LoginResult;
}

#[derive(Debug)]
pub struct NoAuth();

#[async_trait::async_trait]
impl LoginProvider for NoAuth {
    async fn login(&self, _login_request: LoginRequest) -> LoginResult {
        Err(LoginError::Aborted)
    }
}

#[derive(Debug)]
pub struct StdoutAuth();

#[async_trait::async_trait]
impl LoginProvider for StdoutAuth {
    async fn login(&self, request: LoginRequest) -> LoginResult {
        println!("New oidc login required");
        println!(
            "Open this URL in your browser {} and enter the code {}",
            request.verification_uri(),
            request.user_code(),
        );
        if let Some(u) = request.verification_uri_complete() {
            println!("or go directly to {}", u);
        }
        request.get_token().await
    }
}

#[derive(Debug)]
pub struct OidcClientBuilder {
    url: Url,
    client_id: String,
    login_provider: Option<Arc<dyn LoginProvider>>,
    token_cache: Option<PathBuf>,
}

impl OidcClientBuilder {
    pub fn new<C: Into<String>>(url: Url, client_id: C) -> Self {
        Self {
            url,
            client_id: client_id.into(),
            login_provider: None,
            token_cache: None,
        }
    }

    pub fn token_cache(&mut self, token_cache: PathBuf) {
        self.token_cache = Some(token_cache);
    }

    pub fn login_provider<LP: Into<Arc<dyn LoginProvider + 'static>>>(
        &mut self,
        login_provider: LP,
    ) {
        self.login_provider = Some(login_provider.into());
    }

    pub fn build(self) -> OidcClient {
        OidcClient {
            url: self.url,
            client_id: self.client_id,
            login_provider: self.login_provider.unwrap_or(Arc::new(NoAuth {})),
            client: None,
            token: None,
            token_cache: self.token_cache,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OidcClient {
    url: Url,
    client_id: String,
    login_provider: Arc<dyn LoginProvider>,
    client: Option<BasicClient>,
    token: Option<Token>,
    token_cache: Option<PathBuf>,
}

impl OidcClient {
    async fn client(&mut self) -> Option<BasicClient> {
        if self.client.is_none() {
            let discovery = OidcDiscovery::from_uri(self.url.clone()).await.unwrap();
            let device_auth_url =
                DeviceAuthorizationUrl::new(discovery.device_authorization_endpoint).unwrap();
            let client = BasicClient::new(
                ClientId::new(self.client_id.clone()),
                None,
                AuthUrl::new(discovery.authorization_endpoint).unwrap(),
                Some(TokenUrl::new(discovery.token_endpoint).unwrap()),
            )
            .set_device_authorization_url(device_auth_url);
            self.client = Some(client);
        }
        self.client.clone()
    }

    async fn update_token(&mut self, token: oauth2::basic::BasicTokenResponse) {
        let expires = token.expires_in().map(|d| SystemTime::now() + d);
        let old_token = self.token.take();
        let token = Token {
            refresh: token
                .refresh_token()
                .cloned()
                .or_else(|| old_token.and_then(|o| o.refresh)),
            expires,
            access: token.access_token().secret().to_string(),
        };
        if let Some(cache) = &self.token_cache {
            // Don't care if the write fails it's just a cache
            let _ = token.to_file(cache).await;
        }
        self.token = Some(token);
    }

    #[tracing::instrument(skip(self))]
    pub async fn auth(&mut self) -> Result<()> {
        let client = self.client().await.unwrap();
        let details: StandardDeviceAuthorizationResponse = client
            .exchange_device_code()?
            .add_scope(Scope::new("read".to_string()))
            .request_async(async_http_client)
            .await?;

        let request = LoginRequest {
            client: client.clone(),
            details,
        };

        info!("Requesting login from provider");
        let token = self.login_provider.login(request).await?;

        self.update_token(token).await;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn refresh(&mut self) -> Result<()> {
        let client = self.client().await.unwrap();
        if let Some(refresh) = self.token.as_ref().and_then(|t| t.refresh.as_ref()) {
            debug!("Refreshing token");
            match client
                .exchange_refresh_token(refresh)
                .request_async(async_http_client)
                .await
            {
                Ok(token) => {
                    self.update_token(token).await;
                    return Ok(());
                }
                Err(e) => match e {
                    oauth2::RequestTokenError::ServerResponse(srv) => match srv.error() {
                        oauth2::basic::BasicErrorResponseType::InvalidGrant => {
                            debug!("Refresh token not valid, reauthenticating");
                        }
                        e => warn!("Server reported error {}", e),
                    },
                    _ => debug!("Refresh failed reauthing: {}", e),
                },
            }
        }

        self.auth().await?;
        Ok(())
    }

    pub async fn access_token(&mut self) -> Result<&str> {
        if self.token.is_none() {
            if let Some(cache) = &self.token_cache {
                self.token = Token::from_file(cache).await.ok();
            }
        }

        if let Some(token) = &self.token {
            if let Some(expires) = token.expires {
                if expires < SystemTime::now() {
                    debug!("Token expired");
                    self.refresh().await?
                }
            }
        } else {
            self.auth().await?;
        }
        if let Some(token) = self.token.as_ref().map(|t| &t.access) {
            Ok(token)
        } else {
            bail!("Failed to get access token");
        }
    }
}
