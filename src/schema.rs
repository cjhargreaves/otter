use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct App {
    pub name: String,
    pub image: String,
    pub port: u16,
    #[serde(default = "default_instance")]
    pub instance: String,
    #[serde(default = "default_region")]
    pub region: String,
    pub volume: Option<Volume>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub dynamodb: BTreeMap<String, Table>,
}

#[derive(Debug, Deserialize)]
pub struct Volume {
    pub size_gb: i32,
    #[serde(default = "default_mount")]
    pub mount: String,
}

#[derive(Debug, Deserialize)]
pub struct Table {
    pub hash_key: String,
}

fn default_instance() -> String {
    "t4g.small".to_string()
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_mount() -> String {
    "/data".to_string()
}

impl App {
    pub fn load(path: &Path) -> Result<App> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let app: App = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let valid_name = !app.name.is_empty()
            && app
                .name
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        if !valid_name {
            bail!("name must be lowercase letters, digits, and dashes");
        }
        for (key, table) in &app.dynamodb {
            if key.is_empty() || table.hash_key.is_empty() {
                bail!("dynamodb tables need a name and a hash_key");
            }
        }
        Ok(app)
    }

    pub fn table_name(&self, key: &str) -> String {
        format!("{}-{}", self.name, key)
    }

    pub fn arch(&self) -> &'static str {
        let family = self.instance.split('.').next().unwrap_or("");
        if family.ends_with('g') {
            "arm64"
        } else {
            "x86_64"
        }
    }

    pub fn container_env(&self) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        env.insert("AWS_REGION".to_string(), self.region.clone());
        env.insert("PORT".to_string(), self.port.to_string());
        for key in self.dynamodb.keys() {
            let var = format!("OTTER_DYNAMODB_{}", key.to_ascii_uppercase().replace('-', "_"));
            env.insert(var, self.table_name(key));
        }
        env
    }
}
