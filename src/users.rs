use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde_json::{Map, Value, from_reader};

#[derive(Debug)]
pub struct User {
    pub uid: u32,
    pub name: String,
    pub uuid: String,
}

pub fn load_users(json_path: &Path) -> Vec<User> {
    let file = File::open(json_path).unwrap();
    let reader = BufReader::new(file);
    let users: Map<String, Value> = from_reader(reader).unwrap();
    users
        .into_iter()
        .map(|(name, user)| {
            let uid = user["id"].as_u64().unwrap() as u32;
            let Value::Object(mut user) = user else {
                panic!("Invalid user: {:?}", user);
            };
            let Value::String(uuid) = user.get_mut("uuid").unwrap().take() else {
                panic!("Invalid uuid: {:?}", user);
            };
            User { uid, name, uuid }
        })
        .collect()
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
