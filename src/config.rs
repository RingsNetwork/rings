use std::fs::File;
use std::io::prelude::*;
use toml;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub answer: HostConfig,
    pub offer: HostConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HostConfig {
    pub addr: String,
    pub port: u32,
}

pub fn read_config(filename: &str) -> Config {
    let mut config_file = File::open(filename).expect("Config file not found");
    let mut toml_str = String::new();
    config_file
        .read_to_string(&mut toml_str)
        .expect("Failure while reading config file");
    let config: Config = toml::from_str(&toml_str).unwrap();
    return config;
}
