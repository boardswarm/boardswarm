#![allow(dead_code)]
use std::{collections::HashMap, path::Path, time::Duration};

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
    pub modes: Vec<Mode>,
    #[serde(default)]
    pub uploaders: Vec<Uploader>,
}

#[derive(Debug, Deserialize)]
pub struct Console {
    pub name: String,
    #[serde(default)]
    pub default: bool,
    pub parameters: serde_yaml::Value,
    #[serde(rename = "match")]
    pub match_: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Uploader {
    pub name: String,
    #[serde(rename = "match")]
    pub match_: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Mode {
    pub name: String,
    pub depends: Option<String>,
    pub sequence: Vec<ModeStep>,
}

#[derive(Debug, Deserialize)]
pub struct ModeStep {
    #[serde(rename = "match")]
    pub match_: HashMap<String, String>,
    pub parameters: serde_yaml::Value,
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub stabilisation: Option<Duration>,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Config> {
        let file = std::fs::File::open(path)?;
        Ok(serde_yaml::from_reader(file)?)
    }
}
