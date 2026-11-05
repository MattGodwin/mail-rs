//! Local recovery copies of open compose windows. Every edited message is
//! written to disk a moment after you stop typing, so closing the app,
//! a crash or a power cut never loses what you wrote. Files are removed when
//! the message is sent or deliberately discarded.

use crate::config::config_dir;
use crate::store::write_atomic;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// An attachment carried in a compose window: (name, mime, bytes).
pub type Carried = (String, String, Vec<u8>);

#[derive(Serialize, Deserialize, Clone)]
pub struct Saved {
    pub key: String,
    pub version: u32,
    pub acct_id: String,
    #[serde(default)]
    pub from: usize,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub files: Vec<PathBuf>,
    /// (name, mime) of carried attachments; bytes live in sidecar files.
    pub carried: Vec<(String, String)>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    /// (account id, folder, uid)
    pub reply_of: Option<(String, String, u32)>,
    pub replaces: Option<(String, u32, u32)>,
}

fn dir() -> PathBuf {
    config_dir().join("compose")
}

fn safe(key: &str) -> String {
    key.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// Write the recovery copy. Attachment bytes are rewritten only when `carried` is given.
pub fn save(s: &Saved, carried: Option<&[Carried]>) -> Result<(), String> {
    let k = safe(&s.key);
    if let Some(list) = carried {
        for (i, (_, _, data)) in list.iter().enumerate() {
            write_atomic(&dir().join(format!("{k}.{i}.bin")), data).map_err(|e| e.to_string())?;
        }
        // Remove sidecars for attachments that were taken off.
        let mut i = list.len();
        while dir().join(format!("{k}.{i}.bin")).exists() {
            let _ = fs::remove_file(dir().join(format!("{k}.{i}.bin")));
            i += 1;
        }
    }
    let json = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    write_atomic(&dir().join(format!("{k}.json")), &json).map_err(|e| e.to_string())
}

pub fn load_all() -> Vec<(Saved, Vec<Carried>)> {
    let Ok(rd) = fs::read_dir(dir()) else { return vec![] };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(s) = fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok()) else { continue };
        let k = safe(&s.key);
        let carried = s
            .carried
            .iter()
            .enumerate()
            .filter_map(|(i, (n, m))| Some((n.clone(), m.clone(), fs::read(dir().join(format!("{k}.{i}.bin"))).ok()?)))
            .collect();
        out.push((s, carried));
    }
    out
}

pub fn delete(key: &str) {
    let k = safe(key);
    let _ = fs::remove_file(dir().join(format!("{k}.json")));
    let mut i = 0;
    while fs::remove_file(dir().join(format!("{k}.{i}.bin"))).is_ok() {
        i += 1;
    }
}
