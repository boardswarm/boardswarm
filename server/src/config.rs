#![allow(dead_code)]
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub providers: Vec<Provider>,
    pub devices: Vec<Device>,
}

#[derive(Debug, Deserialize)]
pub struct Provider {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub parameters: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Device {
    pub name: String,
    pub consoles: Vec<Console>,
}

#[derive(Debug, Deserialize)]
pub struct Console {
    pub name: String,
    pub parameters: serde_yaml::Value,
    #[serde(rename = "match")]
    pub match_: Match,
}

#[derive(Debug, Deserialize)]
pub struct Match {
    pub provider: String,
    pub filter: serde_yaml::Value,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Config> {
        let file = std::fs::File::open(path)?;
        Ok(serde_yaml::from_reader(file)?)
    }
}
