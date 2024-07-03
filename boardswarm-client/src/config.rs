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

#[derive(Clone, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConfigFile {
    servers: Vec<Server>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

    fn from_slice(slice: &[u8], path: PathBuf) -> Result<Config, Error> {
        let config = serde_yaml::from_slice(slice)?;
        Ok(Config { path, config })
    }

    pub async fn from_file<P: AsRef<Path>>(path: P) -> Result<Config, Error> {
        let content = tokio::fs::read(&path).await?;
        Self::from_slice(&content, path.as_ref().into())
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

    pub fn remove_server(&mut self, name: &str) {
        self.config.servers.retain(|s| s.name != name);
    }

    // Set default server by name; If name is not in the list this is a noop
    pub fn set_default(&mut self, name: &str) {
        if let Some(index) = self.config.servers.iter().position(|s| s.name == name) {
            self.config.servers.swap(0, index)
        }
    }

    /// Returns an iterator to a collection of Servers.
    /// The default server is always first in the iterator.
    pub fn servers(&self) -> impl Iterator<Item = &Server> {
        self.config.servers.iter()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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

fn trim_whitespace<'de, D>(de: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(de)?;
    Ok(s.trim_start().trim_end().to_string())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Auth {
    // If the token ended up in the config file with whitespace (e.g. trialing newline) simply cut
    // that of on deserialization
    Token(#[serde(deserialize_with = "trim_whitespace")] String),
    Oidc {
        uri: url::Url,
        client_id: String,
        token_cache: PathBuf,
    },
}

#[test]
fn token_parsing() {
    let p = PathBuf::from("config.yaml");
    let mut expected = Config::new(p.clone());
    expected.config.servers = vec![Server {
        name: String::from("server1"),
        uri: "https://example.com:6683".parse().unwrap(),
        auth: Auth::Token(String::from("tokentokentoken")),
    }];

    let t = b"
servers:
- name: server1
  uri: https://example.com:6683
  auth: !Token tokentokentoken
";
    let config = Config::from_slice(t, p.clone()).unwrap();
    assert_eq!(config, expected, "normal");

    let t = b"
servers:
- name: server1
  uri: https://example.com:6683
  auth: !Token
    tokentokentoken

";
    let config = Config::from_slice(t, p.clone()).unwrap();
    assert_eq!(config, expected, "trailing whitespace");

    let t = b"
servers:
- name: server1
  uri: https://example.com:6683
  auth: !Token |

    tokentokentoken
";
    let config = Config::from_slice(t, p.clone()).unwrap();
    assert_eq!(config, expected, "leading whitespace");
}
