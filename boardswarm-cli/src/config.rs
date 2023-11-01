use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::client::BoardswarmBuilder;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO error")]
    IO(#[from] std::io::Error),
    #[error("Yaml error")]
    Yaml(#[from] serde_yaml::Error),
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct ConfigFile {
    servers: Vec<Server>,
}

#[derive(Clone, Debug)]
pub struct Config {
    path: PathBuf,
    config: ConfigFile,
}

impl Config {
    pub fn new(path: PathBuf) -> Self {
        Config {
            path,
            config: ConfigFile::default(),
        }
    }

    pub async fn from_file<P: AsRef<Path>>(path: P) -> Result<Config, Error> {
        let content = tokio::fs::read(&path).await?;
        let config = serde_yaml::from_slice(&content)?;
        Ok(Config {
            path: path.as_ref().into(),
            config,
        })
    }

    pub async fn write(&self) -> Result<(), Error> {
        let contents = serde_yaml::to_string(&self.config)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?
        }
        tokio::fs::write(&self.path, contents).await?;
        Ok(())
    }

    pub fn default_server(&self) -> Option<&Server> {
        self.config.servers.first()
    }

    pub fn default_server_mut(&mut self) -> Option<&mut Server> {
        self.config.servers.first_mut()
    }

    pub fn to_boardswarm_builder(&self) -> Option<BoardswarmBuilder> {
        self.default_server().map(|s| s.to_boardswarm_builder())
    }

    pub fn find_server(&self, name: &str) -> Option<&Server> {
        self.config.servers.iter().find(|s| s.name == name)
    }

    pub fn find_server_mut(&mut self, name: &str) -> Option<&mut Server> {
        self.config.servers.iter_mut().find(|s| s.name == name)
    }

    pub fn add_server(&mut self, server: Server) {
        self.config.servers.push(server);
    }

    // Set default server by name; If name is not in the list this is a noop
    pub fn set_default(&mut self, name: &str) {
        if let Some(index) = self.config.servers.iter().position(|s| s.name == name) {
            self.config.servers.swap(0, index)
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Server {
    pub name: String,
    #[serde(with = "http_serde::uri")]
    pub uri: http::Uri,
    pub auth: Auth,
}

impl Server {
    pub fn to_boardswarm_builder(&self) -> BoardswarmBuilder {
        let mut b = BoardswarmBuilder::new(self.uri.clone());
        b.auth(self.auth.clone());
        b
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Auth {
    Token(String),
    Oidc {
        uri: url::Url,
        client_id: String,
        token_cache: PathBuf,
    },
}
