use std::{sync::Arc, task::Poll};

use futures::future::BoxFuture;
use tokio::sync::Mutex;
use tonic::body::BoxBody;
use tower::{Layer, Service};

use crate::oidc::OidcClient;

#[derive(Clone, Debug, Default)]
pub enum Authenticator {
    #[default]
    NoAuth,
    Token(String),
    Oidc(Arc<Mutex<OidcClient>>),
}

impl Authenticator {
    pub fn from_static<S: Into<String>>(token: S) -> Self {
        Self::Token(token.into())
    }

    pub fn from_oidc(oidc: OidcClient) -> Self {
        Self::Oidc(Arc::new(Mutex::new(oidc)))
    }

    pub async fn token(&self) -> Option<String> {
        match self {
            Self::NoAuth => None,
            Self::Token(ref token) => Some(token.clone()),
            Self::Oidc(ref oidc) => {
                let mut oidc = oidc.lock().await;
                oidc.access_token().await.ok().map(|s| s.to_string())
            }
        }
    }

    pub fn into_layer(self) -> AuthenticatorLayer {
        AuthenticatorLayer {
            authenticator: Arc::new(self),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuthenticatorLayer {
    authenticator: Arc<Authenticator>,
}

impl<S> Layer<S> for AuthenticatorLayer {
    type Service = AuthenticatorService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthenticatorService {
            inner,
            authenticator: self.authenticator.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuthenticatorService<S> {
    inner: S,
    authenticator: Arc<Authenticator>,
}

impl<S> Service<http::Request<BoxBody>> for AuthenticatorService<S>
where
    S: Clone + Service<http::Request<BoxBody>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<BoxBody>) -> Self::Future {
        let auth = self.authenticator.clone();
        let inner = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, inner);

        Box::pin(async move {
            if let Some(token) = auth.token().await {
                req.headers_mut().insert(
                    http::header::AUTHORIZATION.as_str(),
                    format!("Bearer {}", token.trim()).parse().unwrap(),
                );
            }
            inner.call(req).await
        })
    }
}
