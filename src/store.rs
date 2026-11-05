//! Local cache. Messages are stored exactly as the server sent them
//! (`<uidvalidity>-<uid>.eml`), byte for byte — the client never rewrites
//! message content. Folder indexes are small JSON files so the UI can show
//! the last known state instantly at startup.

use crate::model::{FolderIndex, FolderInfo};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

/// File-system safe and case-insensitive-safe encoding of a folder name.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("_{b:02x}")),
        }
    }
    out
}

pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path)
}

impl Store {
    pub fn new(account_id: &str) -> Store {
        Store { root: crate::config::cache_dir().join(sanitize(account_id)) }
    }

    fn dir(&self, folder: &str) -> PathBuf {
        self.root.join(sanitize(folder))
    }

    pub fn load_index(&self, folder: &str) -> Option<FolderIndex> {
        let b = fs::read(self.dir(folder).join("index.json")).ok()?;
        serde_json::from_slice(&b).ok()
    }

    pub fn save_index(&self, folder: &str, idx: &FolderIndex) {
        if let Ok(json) = serde_json::to_vec(idx) {
            let _ = write_atomic(&self.dir(folder).join("index.json"), &json);
        }
    }

    fn body_path(&self, folder: &str, uidvalidity: u32, uid: u32) -> PathBuf {
        self.dir(folder).join(format!("{uidvalidity}-{uid}.eml"))
    }

    pub fn load_body(&self, folder: &str, uidvalidity: u32, uid: u32) -> Option<Vec<u8>> {
        fs::read(self.body_path(folder, uidvalidity, uid)).ok()
    }

    pub fn has_body(&self, folder: &str, uidvalidity: u32, uid: u32) -> bool {
        self.body_path(folder, uidvalidity, uid).exists()
    }

    pub fn save_body(&self, folder: &str, uidvalidity: u32, uid: u32, raw: &[u8]) {
        let _ = write_atomic(&self.body_path(folder, uidvalidity, uid), raw);
    }

    pub fn remove_body(&self, folder: &str, uidvalidity: u32, uid: u32) {
        let _ = fs::remove_file(self.body_path(folder, uidvalidity, uid));
    }

    /// Drop everything cached for a folder (e.g. after UIDVALIDITY changed).
    pub fn clear_folder(&self, folder: &str) {
        let _ = fs::remove_dir_all(self.dir(folder));
    }

    pub fn load_folders(&self) -> Option<Vec<FolderInfo>> {
        let b = fs::read(self.root.join("folders.json")).ok()?;
        serde_json::from_slice(&b).ok()
    }

    pub fn save_folders(&self, folders: &[FolderInfo]) {
        if let Ok(json) = serde_json::to_vec(folders) {
            let _ = write_atomic(&self.root.join("folders.json"), &json);
        }
    }

    /// Remove the whole account cache (account deleted).
    pub fn purge(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sanitize_is_case_safe() {
        assert_ne!(super::sanitize("Inbox"), super::sanitize("INBOX"));
        assert_eq!(super::sanitize("a/b"), "a_2fb");
    }
}
