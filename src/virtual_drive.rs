use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

pub const DEFAULT_DRIVE_LETTER: char = 'X';
const MANIFEST_FILE_NAME: &str = ".clouddrive-manifest.json";

#[derive(Clone, Debug)]
pub struct LocalFileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified_unix_secs: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncManifest {
    pub entries: BTreeMap<String, ManifestEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub size: u64,
    pub modified_unix_secs: u64,
}

pub fn drive_label(letter: char) -> String {
    format!("CloudDrive ({}:)", normalize_drive_letter(letter))
}

pub fn status_text(letter: char) -> String {
    if is_mounted(letter) {
        format!("Mounted at {}", drive_label(letter))
    } else {
        format!("Drive offline ({}: available)", normalize_drive_letter(letter))
    }
}

pub fn ensure_cache_root() -> Result<PathBuf, String> {
    let root = cache_root();
    fs::create_dir_all(&root).map_err(|err| format!("failed to create cache root: {err}"))?;
    Ok(root)
}

pub fn collect_local_files(root: &Path) -> Result<BTreeMap<String, LocalFileEntry>, String> {
    let mut entries = BTreeMap::new();
    if !root.exists() {
        return Ok(entries);
    }

    collect_recursive(root, root, &mut entries)?;
    Ok(entries)
}

pub fn load_manifest(root: &Path) -> SyncManifest {
    let manifest_path = root.join(MANIFEST_FILE_NAME);
    let Ok(content) = fs::read_to_string(manifest_path) else {
        return SyncManifest::default();
    };

    serde_json::from_str(&content).unwrap_or_default()
}

pub fn save_manifest(root: &Path, manifest: &SyncManifest) -> Result<(), String> {
    let manifest_path = root.join(MANIFEST_FILE_NAME);
    let content = serde_json::to_string_pretty(manifest)
        .map_err(|err| format!("failed to serialize manifest: {err}"))?;
    fs::write(manifest_path, content).map_err(|err| format!("failed to write manifest: {err}"))
}

pub fn manifest_from_local_files(local_files: &BTreeMap<String, LocalFileEntry>) -> SyncManifest {
    let entries = local_files
        .iter()
        .map(|(key, file)| {
            (
                key.clone(),
                ManifestEntry {
                    size: file.size,
                    modified_unix_secs: file.modified_unix_secs,
                },
            )
        })
        .collect();

    SyncManifest { entries }
}

pub fn key_to_cache_path(root: &Path, key: &str) -> Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    let components = normalized_key_components(key)?;
    if components.is_empty() {
        return Err(format!("unsupported object key path: {key}"));
    }

    for component in components {
        path.push(component);
    }
    Ok(path)
}

pub fn normalized_key_components(key: &str) -> Result<Vec<String>, String> {
    let normalized = key.trim().replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err("object key is empty".to_string());
    }

    let mut components = Vec::new();
    for raw_part in trimmed.split('/') {
        let part = raw_part.trim();
        if part.is_empty() || part == "." {
            continue;
        }

        if part == ".." {
            return Err("parent traversal is not allowed".to_string());
        }

        if has_windows_drive_prefix(part) {
            return Err("drive-prefixed paths are not allowed".to_string());
        }

        let sanitized = sanitize_windows_component(part);
        if sanitized.is_empty() {
            return Err(format!("path component '{part}' becomes empty after sanitization"));
        }

        components.push(sanitized);
    }

    if components.is_empty() {
        Err("object key does not contain a usable path".to_string())
    } else {
        Ok(components)
    }
}

pub fn remove_stale_local_files(root: &Path, keep_keys: &BTreeSet<String>) -> Result<(), String> {
    let local_files = collect_local_files(root)?;
    for (key, entry) in local_files {
        if keep_keys.contains(&key) {
            continue;
        }

        if entry.path.exists() {
            fs::remove_file(&entry.path)
                .map_err(|err| format!("failed to remove stale file {}: {err}", entry.path.display()))?;
        }
    }

    prune_empty_dirs(root)?;
    Ok(())
}

pub fn mount_drive(letter: char, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        if !target.exists() {
            fs::create_dir_all(target)
                .map_err(|err| format!("failed to create drive target: {err}"))?;
        }

        let normalized_letter = normalize_drive_letter(letter);
        let drive = format!("{normalized_letter}:");
        if is_mounted(normalized_letter) {
            return Ok(());
        }

        if is_drive_letter_in_use(normalized_letter) {
            return Err(format!(
                "drive letter {normalized_letter}: is already in use by Windows or another app"
            ));
        }

        let target = target
            .canonicalize()
            .map_err(|err| format!("failed to resolve drive target {}: {err}", target.display()))?;

        let output = Command::new("subst")
            .arg(&drive)
            .arg(&target)
            .output()
            .map_err(|err| format!("failed to launch subst: {err}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure_message(
                &format!("failed to mount virtual drive at {drive}"),
                &output.stdout,
                &output.stderr,
            ))
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (letter, target);
        Err("virtual drive mounting is only supported on Windows".to_string())
    }
}

pub fn unmount_drive(letter: char) -> Result<(), String> {
    #[cfg(windows)]
    {
        let normalized_letter = normalize_drive_letter(letter);
        let drive = format!("{normalized_letter}:");
        if !is_mounted(normalized_letter) {
            return Ok(());
        }

        let output = Command::new("subst")
            .arg(&drive)
            .arg("/D")
            .output()
            .map_err(|err| format!("failed to launch subst: {err}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure_message(
                &format!("failed to unmount virtual drive at {drive}"),
                &output.stdout,
                &output.stderr,
            ))
        }
    }

    #[cfg(not(windows))]
    {
        let _ = letter;
        Err("virtual drive unmounting is only supported on Windows".to_string())
    }
}

pub fn is_mounted(letter: char) -> bool {
    #[cfg(windows)]
    {
        let drive = format!("{}:", normalize_drive_letter(letter));
        let Ok(output) = Command::new("subst").output() else {
            return false;
        };

        if !output.status.success() {
            return false;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().any(|line| line.trim_start().starts_with(&drive))
    }

    #[cfg(not(windows))]
    {
        let _ = letter;
        false
    }
}

fn cache_root() -> PathBuf {
    if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
        PathBuf::from(local_app_data)
            .join("CloudDrive")
            .join("mirror-cache")
    } else {
        PathBuf::from(".cloud-drive-cache")
    }
}

fn normalize_drive_letter(letter: char) -> char {
    letter.to_ascii_uppercase()
}

pub fn preferred_drive_letter() -> char {
    DEFAULT_DRIVE_LETTER
}

fn collect_recursive(
    root: &Path,
    current: &Path,
    entries: &mut BTreeMap<String, LocalFileEntry>,
) -> Result<(), String> {
    for item in fs::read_dir(current)
        .map_err(|err| format!("failed to read directory {}: {err}", current.display()))?
    {
        let item = item.map_err(|err| format!("failed to inspect directory entry: {err}"))?;
        let path = item.path();
        let file_name = item.file_name();
        if file_name.to_string_lossy() == MANIFEST_FILE_NAME {
            continue;
        }

        let metadata = item
            .metadata()
            .map_err(|err| format!("failed to read metadata for {}: {err}", path.display()))?;

        if metadata.is_dir() {
            collect_recursive(root, &path, entries)?;
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|err| format!("failed to compute relative path for {}: {err}", path.display()))?;
        let key = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        let modified_unix_secs = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);

        entries.insert(
            key,
            LocalFileEntry {
                path,
                size: metadata.len(),
                modified_unix_secs,
            },
        );
    }

    Ok(())
}

fn prune_empty_dirs(root: &Path) -> Result<bool, String> {
    if !root.exists() || !root.is_dir() {
        return Ok(false);
    }

    let mut contains_entries = false;
    for entry in fs::read_dir(root)
        .map_err(|err| format!("failed to read directory {}: {err}", root.display()))?
    {
        let entry = entry.map_err(|err| format!("failed to inspect directory entry: {err}"))?;
        let path = entry.path();

        if path.is_dir() {
            let child_has_entries = prune_empty_dirs(&path)?;
            if !child_has_entries {
                fs::remove_dir(&path)
                    .map_err(|err| format!("failed to remove empty directory {}: {err}", path.display()))?;
            } else {
                contains_entries = true;
            }
            continue;
        }

        if entry.file_name().to_string_lossy() == MANIFEST_FILE_NAME {
            continue;
        }

        contains_entries = true;
    }

    Ok(contains_entries)
}

fn has_windows_drive_prefix(part: &str) -> bool {
    let bytes = part.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn sanitize_windows_component(part: &str) -> String {
    let mut sanitized = part
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>();

    sanitized = sanitized.trim().trim_end_matches('.').to_string();
    if sanitized.is_empty() {
        return String::new();
    }

    let uppercase = sanitized.to_ascii_uppercase();
    if is_windows_reserved_name(&uppercase) {
        sanitized.push('_');
    }

    sanitized
}

fn is_windows_reserved_name(name: &str) -> bool {
    matches!(
        name,
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn is_drive_letter_in_use(letter: char) -> bool {
    #[cfg(windows)]
    {
        let drive_root = format!("{}:\\", normalize_drive_letter(letter));
        Path::new(&drive_root).exists()
    }

    #[cfg(not(windows))]
    {
        let _ = letter;
        false
    }
}

fn command_failure_message(prefix: &str, stdout: &[u8], stderr: &[u8]) -> String {
    let stderr_message = String::from_utf8_lossy(stderr).trim().to_string();
    let stdout_message = String::from_utf8_lossy(stdout).trim().to_string();
    let message = if !stderr_message.is_empty() {
        stderr_message
    } else {
        stdout_message
    };

    if message.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {message}")
    }
}
