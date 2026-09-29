//! Web bundles the host serves to a WebView from its own files.
//!
//! A bundle is a directory with a `manifest.json` that lists every file the
//! page may load, with its SHA-256 and size. Opening a bundle checks every
//! listed file and keeps the verified bytes in memory; the host then serves
//! exactly those bytes and nothing else. The manifest carries a protocol
//! version, and readers ignore keys they do not know, so later manifests can
//! add fields.
//!
//! ```json
//! {
//!   "protocol_version": 1,
//!   "name": "bridge-test",
//!   "files": {
//!     "index.html": { "sha256": "…", "size": 1432, "content_type": "text/html; charset=utf-8" }
//!   }
//! }
//! ```

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::MobileError;

/// The manifest format this build reads and writes.
pub const BUNDLE_PROTOCOL_VERSION: u32 = 1;
pub const MANIFEST_FILE: &str = "manifest.json";
const MAX_FILES: usize = 4096;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    protocol_version: u32,
    #[serde(default)]
    name: Option<String>,
    files: BTreeMap<String, ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    sha256: String,
    size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BundleResponse {
    pub path: String,
    pub content_type: String,
    pub body: Vec<u8>,
    pub sha256: String,
}

struct VerifiedFile {
    content_type: String,
    body: Arc<Vec<u8>>,
    sha256: String,
}

#[derive(uniffi::Object)]
pub struct WebBundle {
    name: String,
    protocol_version: u32,
    files: BTreeMap<String, VerifiedFile>,
}

fn reject(reason: impl Into<String>) -> MobileError {
    MobileError::Bundle {
        reason: reason.into(),
    }
}

/// A manifest path must be relative, use `/` separators and name no parent,
/// current or empty segment.
fn check_path(path: &str) -> Result<(), MobileError> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path == MANIFEST_FILE
        || path
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if bad {
        return Err(reject(format!("unsafe path in manifest: {path:?}")));
    }
    Ok(())
}

/// Every component from the bundle root to the file must be a real directory
/// or file, never a symlink, so a link cannot pull in files from outside.
fn check_no_symlinks(root: &Path, relative: &str) -> Result<PathBuf, MobileError> {
    let mut current = root.to_path_buf();
    for segment in relative.split('/') {
        current.push(segment);
        let meta =
            std::fs::symlink_metadata(&current).map_err(|e| reject(format!("{relative}: {e}")))?;
        if meta.file_type().is_symlink() {
            return Err(reject(format!("{relative} goes through a symlink")));
        }
    }
    Ok(current)
}

pub(crate) fn content_type_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Decode `%XX` escapes in a URL path. Invalid escapes are kept literally, and
/// a lookup with them simply finds nothing.
fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl WebBundle {
    fn load(dir: &Path) -> Result<Self, MobileError> {
        let manifest_bytes = std::fs::read(dir.join(MANIFEST_FILE))
            .map_err(|e| reject(format!("cannot read {MANIFEST_FILE}: {e}")))?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| reject(format!("{MANIFEST_FILE} is not a valid manifest: {e}")))?;
        if manifest.protocol_version != BUNDLE_PROTOCOL_VERSION {
            return Err(reject(format!(
                "manifest protocol version {} is not supported; this host reads version {BUNDLE_PROTOCOL_VERSION}",
                manifest.protocol_version
            )));
        }
        if manifest.files.len() > MAX_FILES {
            return Err(reject(format!("more than {MAX_FILES} files")));
        }
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        for (path, entry) in &manifest.files {
            check_path(path)?;
            let full = check_no_symlinks(dir, path)?;
            total = total.saturating_add(entry.size);
            if total > MAX_TOTAL_BYTES {
                return Err(reject(format!(
                    "the bundle is larger than {MAX_TOTAL_BYTES} bytes"
                )));
            }
            let body = std::fs::read(&full).map_err(|e| reject(format!("{path}: {e}")))?;
            if body.len() as u64 != entry.size {
                return Err(reject(format!(
                    "{path} is {} bytes, the manifest says {}",
                    body.len(),
                    entry.size
                )));
            }
            let digest = sha256_hex(&body);
            if !digest.eq_ignore_ascii_case(&entry.sha256) {
                return Err(reject(format!(
                    "{path} does not match its SHA-256 in the manifest"
                )));
            }
            let content_type = entry
                .content_type
                .clone()
                .unwrap_or_else(|| content_type_for(path).to_owned());
            files.insert(
                path.clone(),
                VerifiedFile {
                    content_type,
                    body: Arc::new(body),
                    sha256: digest,
                },
            );
        }
        if !files.contains_key("index.html") {
            return Err(reject("the manifest lists no index.html"));
        }
        Ok(Self {
            name: manifest.name.unwrap_or_else(|| "bundle".into()),
            protocol_version: manifest.protocol_version,
            files,
        })
    }

    /// The manifest path for a request path such as `/app.js?v=2`.
    pub(crate) fn lookup_path(url_path: &str) -> String {
        let path = url_path.split(['?', '#']).next().unwrap_or("");
        let path = percent_decode(path);
        let path = path.trim_start_matches('/');
        if path.is_empty() || path.ends_with('/') {
            format!("{path}index.html")
        } else {
            path.to_owned()
        }
    }
}

#[uniffi::export]
impl WebBundle {
    /// Open and verify the bundle in `dir`.
    #[uniffi::constructor]
    pub fn open(dir: String) -> Result<Arc<Self>, MobileError> {
        Self::load(Path::new(&dir)).map(Arc::new)
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    /// Every path the bundle serves.
    pub fn files(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    /// The verified file for a request path, or `None` for anything the
    /// manifest does not list.
    pub fn resolve(&self, url_path: String) -> Option<BundleResponse> {
        let path = Self::lookup_path(&url_path);
        self.files.get(&path).map(|file| BundleResponse {
            path,
            content_type: file.content_type.clone(),
            body: file.body.as_ref().clone(),
            sha256: file.sha256.clone(),
        })
    }
}

/// Write `manifest.json` for every file below `dir` (except dotfiles and the
/// manifest itself). Returns the manifest text.
#[uniffi::export]
pub fn write_bundle_manifest(dir: String, name: String) -> Result<String, MobileError> {
    let root = PathBuf::from(&dir);
    let mut files = BTreeMap::new();
    let mut stack = vec![root.clone()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|e| reject(format!("{}: {e}", current.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| reject(e.to_string()))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| reject(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(&root)
                .map_err(|e| reject(e.to_string()))?
                .components()
                .map(|c| match c {
                    Component::Normal(part) => Ok(part.to_string_lossy().into_owned()),
                    _ => Err(reject(format!("unexpected path {}", path.display()))),
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            if relative == MANIFEST_FILE {
                continue;
            }
            let body = std::fs::read(&path).map_err(|e| reject(e.to_string()))?;
            files.insert(
                relative.clone(),
                ManifestEntry {
                    sha256: sha256_hex(&body),
                    size: body.len() as u64,
                    content_type: Some(content_type_for(&relative).to_owned()),
                },
            );
        }
    }
    let manifest = Manifest {
        protocol_version: BUNDLE_PROTOCOL_VERSION,
        name: Some(name),
        files,
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(MobileError::internal)?;
    std::fs::write(root.join(MANIFEST_FILE), format!("{text}\n")).map_err(MobileError::storage)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<html>hi</html>").unwrap();
        std::fs::create_dir(dir.path().join("js")).unwrap();
        std::fs::write(dir.path().join("js/app.js"), b"console.log(1)").unwrap();
        write_bundle_manifest(dir.path().display().to_string(), "test".into()).unwrap();
        dir
    }

    #[test]
    fn serves_only_listed_verified_files() {
        let dir = bundle_dir();
        std::fs::write(dir.path().join("secret.txt"), b"not in the manifest").unwrap();
        let bundle = WebBundle::open(dir.path().display().to_string()).unwrap();
        assert_eq!(
            bundle.files(),
            vec!["index.html".to_string(), "js/app.js".to_string()]
        );
        let index = bundle.resolve("/".into()).unwrap();
        assert_eq!(index.body, b"<html>hi</html>");
        assert!(index.content_type.starts_with("text/html"));
        assert!(bundle.resolve("/js/app.js?v=2#x".into()).is_some());
        assert!(bundle.resolve("/secret.txt".into()).is_none());
        assert!(bundle.resolve("/manifest.json".into()).is_none());
        assert!(bundle.resolve("/../index.html".into()).is_none());
        assert!(bundle.resolve("/js%2Fapp.js".into()).is_some());
    }

    #[test]
    fn a_changed_file_is_rejected() {
        let dir = bundle_dir();
        std::fs::write(dir.path().join("js/app.js"), b"console.log(2)").unwrap();
        let err = WebBundle::open(dir.path().display().to_string())
            .err()
            .unwrap();
        assert!(err.to_string().contains("app.js"), "{err}");
    }

    #[test]
    fn unknown_manifest_keys_are_ignored() {
        let dir = bundle_dir();
        let path = dir.path().join(MANIFEST_FILE);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["signed_by"] = serde_json::json!("someone");
        manifest["files"]["index.html"]["cache"] = serde_json::json!("forever");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(WebBundle::open(dir.path().display().to_string()).is_ok());
    }

    #[test]
    fn another_protocol_version_is_rejected() {
        let dir = bundle_dir();
        let path = dir.path().join(MANIFEST_FILE);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["protocol_version"] = serde_json::json!(2);
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let err = WebBundle::open(dir.path().display().to_string())
            .err()
            .unwrap();
        assert!(err.to_string().contains("protocol version 2"), "{err}");
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        for path in [
            "/etc/passwd",
            "../x",
            "a//b",
            "a/./b",
            "a\\b",
            "",
            MANIFEST_FILE,
        ] {
            assert!(check_path(path).is_err(), "{path:?} should be rejected");
        }
        assert!(check_path("assets/app.js").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected() {
        let dir = bundle_dir();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.js"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("x.js"), dir.path().join("link.js"))
            .unwrap();
        let path = dir.path().join(MANIFEST_FILE);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["files"]["link.js"] = serde_json::json!({
            "sha256": sha256_hex(b"x"), "size": 1
        });
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let err = WebBundle::open(dir.path().display().to_string())
            .err()
            .unwrap();
        assert!(err.to_string().contains("symlink"), "{err}");
    }
}
