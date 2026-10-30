use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub struct User {
    pub uid: u32,
    pub name: String,
    pub uuid: String,
}

#[derive(Deserialize)]
struct Entry {
    id: u32,
    uuid: String,
}

pub fn load_users(json_path: &Path) -> Result<Vec<User>> {
    let file = File::open(json_path).with_context(|| json_path.display().to_string())?;
    let users: BTreeMap<String, Entry> = serde_json::from_reader(BufReader::new(file))
        .with_context(|| json_path.display().to_string())?;
    Ok(users
        .into_iter()
        .map(|(name, Entry { id, uuid })| User {
            uid: id,
            name,
            uuid,
        })
        .collect())
}

impl User {
    pub fn name_from_xray_email(email: &str) -> &str {
        // email before @
        email.split('@').next().unwrap()
    }

    pub fn hysteria_user(&self) -> &str {
        // uuid before the first hyphen
        self.uuid.split('-').next().unwrap()
    }
}
