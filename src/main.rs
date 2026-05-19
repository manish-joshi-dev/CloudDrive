use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
};

use reqwest::multipart::{Form, Part};
use slint::{ModelRc, SharedString, VecModel};
use walkdir::WalkDir;

slint::include_modules!();
mod server;
mod virtual_drive;

#[derive(Clone, Debug)]
struct ObjectEntry {
    key: String,
    size: String,
    kind: String,
    modified: String,
}

#[derive(Clone, Debug)]
struct VisibleEntry {
    name: String,
    full_key: String,
    size: String,
    kind: String,
    modified: String,
    is_dir: bool,
}

#[derive(Default)]
struct BrowserState {
    all_objects: Vec<ObjectEntry>,
    visible_entries: Vec<VisibleEntry>,
    prefix: String,
    search: String,
}

fn normalize_prefix(prefix: &str) -> String {
    if prefix.is_empty() {
        String::new()
    } else if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

fn parent_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(idx) => format!("{}/", &trimmed[..idx]),
        None => String::new(),
    }
}

fn matches_search(name: &str, search: &str) -> bool {
    if search.is_empty() {
        true
    } else {
        name.to_ascii_lowercase()
            .contains(&search.to_ascii_lowercase())
    }
}

fn rebuild_visible_entries(state: &mut BrowserState) {
    let prefix = normalize_prefix(&state.prefix);
    state.prefix = prefix.clone();

    let mut folders = BTreeMap::<String, VisibleEntry>::new();
    let mut files = Vec::<VisibleEntry>::new();

    for object in &state.all_objects {
        if !object.key.starts_with(&prefix) {
            continue;
        }
        
        let Some(remainder) = object.key.get(prefix.len()..) else {
            continue;
        };

        if remainder.is_empty() {
            continue;
        }

        if let Some((folder_name, _)) = remainder.split_once('/') {
            if folder_name.is_empty() || !matches_search(folder_name, &state.search) {
                continue;
            }

            folders
                .entry(folder_name.to_string())
                .or_insert_with(|| VisibleEntry {
                    name: folder_name.to_string(),
                    full_key: format!("{prefix}{folder_name}/"),
                    size: String::new(),
                    kind: "DIR".to_string(),
                    modified: String::new(),
                    is_dir: true,
                });
            continue;
        }

        if !matches_search(remainder, &state.search) {
            continue;
        }

        files.push(VisibleEntry {
            name: remainder.to_string(),
            full_key: object.key.clone(),
            size: object.size.clone(),
            kind: object.kind.clone(),
            modified: object.modified.clone(),
            is_dir: false,
        });
    }

    files.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });

    state.visible_entries = folders.into_values().chain(files).collect();
}

fn update_view(app: &AppWindow, state: &BrowserState) {
    let rows: Vec<FileEntry> = state
        .visible_entries
        .iter()
        .map(|entry| FileEntry {
            name: entry.name.clone().into(),
            size: entry.size.clone().into(),
            kind: entry.kind.clone().into(),
            modified: entry.modified.clone().into(),
        })
        .collect();

    let location = if state.prefix.is_empty() {
        "root".to_string()
    } else {
        state.prefix.trim_end_matches('/').to_string()
    };

    let status = format!("{} items in {}", state.visible_entries.len(), location);

    app.set_files(ModelRc::new(VecModel::from(rows)));
    app.set_prefix(state.prefix.clone().into());
    app.set_status_msg(status.into());
    app.set_is_loading(false);
}

fn push_state_to_ui(app_weak: &slint::Weak<AppWindow>, browser_state: &Rc<RefCell<BrowserState>>) {
    if let Some(app) = app_weak.upgrade() {
        let state = browser_state.borrow();
        update_view(&app, &state);
    }
}

async fn fetch_objects() -> Result<Vec<ObjectEntry>, String> {
    let response = reqwest::get("http://localhost:3000/files")
        .await
        .map_err(|err| format!("failed to fetch files: {err}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read response body>".to_string());
        return Err(format!("file list request failed with status {status}: {body}"));
    }

    let parsed = response
        .json::<Vec<serde_json::Value>>()
        .await
        .map_err(|err| format!("failed to parse file list: {err}"))?;

    Ok(parsed
        .into_iter()
        .map(|file| ObjectEntry {
            key: file["name"].as_str().unwrap_or_default().to_string(),
            size: file["size"].as_str().unwrap_or_default().to_string(),
            kind: file["kind"].as_str().unwrap_or_default().to_string(),
            modified: file["modified"].as_str().unwrap_or_default().to_string(),
        })
        .collect())
}

async fn refresh_objects(
    browser_state: Rc<RefCell<BrowserState>>,
    app_weak: slint::Weak<AppWindow>,
) -> Result<(), String> {
    if let Some(app) = app_weak.upgrade() {
        app.set_is_loading(true);
    }

    let objects = fetch_objects().await?;

    {
        let mut state = browser_state.borrow_mut();
        state.all_objects = objects;
        rebuild_visible_entries(&mut state);
    }

    push_state_to_ui(&app_weak, &browser_state);
    Ok(())
}

fn encode_key(key: &str) -> String {
    urlencoding::encode(key).into_owned()
}

fn default_file_name(key: &str) -> String {
    Path::new(key)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download.bin")
        .to_string()
}

async fn upload_selected_file(selected_path: PathBuf, target_key: String) -> Result<(), String> {
    let file_bytes = tokio::fs::read(&selected_path)
        .await
        .map_err(|err| format!("failed to read file: {err}"))?;

    let part = Part::bytes(file_bytes).file_name(target_key);
    let form = Form::new().part("file", part);

    let response = reqwest::Client::new()
        .post("http://localhost:3000/upload")
        .multipart(form)
        .send()
        .await
        .map_err(|err| format!("upload request failed: {err}"))?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("upload failed with status {}", response.status()))
    }
}

fn to_s3_key(prefix: &str, relative_path: &Path) -> String {
    let relative = relative_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    format!("{prefix}{relative}")
}

async fn upload_folder(selected_folder: PathBuf, prefix: String) -> Result<(), String> {
    let folder_name = selected_folder
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "failed to resolve folder name".to_string())?
        .to_string();
    let base_prefix = format!("{prefix}{folder_name}/");
    let mut form = Form::new();
    let mut file_count = 0usize;

    for entry in WalkDir::new(&selected_folder) {
        let entry = entry.map_err(|err| format!("failed to walk folder: {err}"))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path().to_path_buf();
        let relative_path = path
            .strip_prefix(&selected_folder)
            .map_err(|err| format!("failed to build relative path: {err}"))?;
        let key = to_s3_key(&base_prefix, relative_path);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

        form = form.part("file", Part::bytes(bytes).file_name(key));
        file_count += 1;
    }

    if file_count == 0 {
        return Err("selected folder does not contain any files".to_string());
    }

    let response = reqwest::Client::new()
        .post("http://localhost:3000/upload")
        .multipart(form)
        .send()
        .await
        .map_err(|err| format!("folder upload request failed: {err}"))?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("folder upload failed with status {}", response.status()))
    }
}

async fn download_to_path(key: String, save_path: PathBuf) -> Result<(), String> {
    let response = reqwest::get(format!("http://localhost:3000/download/{}", encode_key(&key)))
        .await
        .map_err(|err| format!("download request failed: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("download failed with status {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("failed to read download body: {err}"))?;

    if let Some(parent) = save_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("failed to create directory {}: {err}", parent.display()))?;
    }

    tokio::fs::write(save_path, bytes)
        .await
        .map_err(|err| format!("failed to save file: {err}"))?;

    Ok(())
}

async fn delete_object(key: String) -> Result<(), String> {
    let response = reqwest::Client::new()
        .delete(format!("http://localhost:3000/delete/{}", encode_key(&key)))
        .send()
        .await
        .map_err(|err| format!("delete request failed: {err}"))?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("delete failed with status {}", response.status()))
    }
}

fn set_drive_status(app_weak: &slint::Weak<AppWindow>, message: impl Into<SharedString>) {
    if let Some(app) = app_weak.upgrade() {
        app.set_drive_status(message.into());
    }
}

fn log_drive_error(context: &str, err: &str) {
    eprintln!("[drive:{context}] {err}");
}

fn refresh_drive_status(app_weak: &slint::Weak<AppWindow>) {
    if let Some(app) = app_weak.upgrade() {
        let preferred_letter = virtual_drive::preferred_drive_letter();
        let mounted = virtual_drive::is_mounted(preferred_letter);
        app.set_drive_mounted(mounted);
        app.set_drive_status(virtual_drive::status_text(preferred_letter).into());
    }
}

async fn sync_virtual_drive_cache() -> Result<String, String> {
    let cache_root = virtual_drive::ensure_cache_root()?;
    let drive_letter = virtual_drive::preferred_drive_letter();
    if !virtual_drive::is_mounted(drive_letter) {
        return Err(format!(
            "{} is not mounted yet",
            virtual_drive::drive_label(drive_letter)
        ));
    }

    let manifest = virtual_drive::load_manifest(&cache_root);
    let local_files = virtual_drive::collect_local_files(&cache_root)?;

    for (key, local_entry) in &local_files {
        let unchanged = manifest
            .entries
            .get(key)
            .map(|entry| {
                entry.size == local_entry.size
                    && entry.modified_unix_secs == local_entry.modified_unix_secs
            })
            .unwrap_or(false);

        if unchanged {
            continue;
        }

        upload_selected_file(local_entry.path.clone(), key.clone()).await?;
    }

    let local_keys: BTreeSet<String> = local_files.keys().cloned().collect();
    for key in manifest.entries.keys() {
        if !local_keys.contains(key) {
            delete_object(key.clone()).await?;
        }
    }

    let remote_objects = fetch_objects().await?;
    let mut keep_keys = BTreeSet::new();
    let mut skipped_keys = Vec::new();
    for object in &remote_objects {
        let cache_path = match virtual_drive::key_to_cache_path(&cache_root, &object.key) {
            Ok(path) => path,
            Err(err) => {
                skipped_keys.push(format!("{} ({err})", object.key));
                continue;
            }
        };

        keep_keys.insert(object.key.clone());
        download_to_path(object.key.clone(), cache_path).await?;
    }

    virtual_drive::remove_stale_local_files(&cache_root, &keep_keys)?;

    let refreshed_local_files = virtual_drive::collect_local_files(&cache_root)?;
    let refreshed_manifest = virtual_drive::manifest_from_local_files(&refreshed_local_files);
    virtual_drive::save_manifest(&cache_root, &refreshed_manifest)?;

    let mut message = format!(
        "Synced {} item(s) to {}",
        keep_keys.len(),
        virtual_drive::drive_label(virtual_drive::preferred_drive_letter())
    );

    if !skipped_keys.is_empty() {
        let sample = skipped_keys
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        message.push_str(&format!(
            " | skipped {} invalid key(s): {}",
            skipped_keys.len(),
            sample
        ));
    }

    Ok(message)
}

async fn mount_virtual_drive() -> Result<String, String> {
    let drive_letter = virtual_drive::preferred_drive_letter();
    let cache_root = virtual_drive::ensure_cache_root()?;
    virtual_drive::mount_drive(drive_letter, &cache_root)?;
    let sync_message = sync_virtual_drive_cache().await?;
    Ok(format!(
        "{} | {}",
        sync_message,
        virtual_drive::status_text(drive_letter)
    ))
}

#[tokio::main]
async fn main() {
    let app = AppWindow::new().unwrap();

    tokio::spawn(async {
        server::run().await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let initial_objects = match fetch_objects().await {
        Ok(objects) => objects,
        Err(err) => {
            eprintln!("{err}");
            Vec::new()
        }
    };

    let browser_state = Rc::new(RefCell::new(BrowserState {
        all_objects: initial_objects,
        ..Default::default()
    }));

    {
        let mut state = browser_state.borrow_mut();
        rebuild_visible_entries(&mut state);
        update_view(&app, &state);
    }

    let app_weak = app.as_weak();
    refresh_drive_status(&app_weak);

    {
        let app_weak = app_weak.clone();
        let browser_state = browser_state.clone();
        app.on_activate_entry(move |idx| {
            let mut state = browser_state.borrow_mut();
            if let Some(entry) = state.visible_entries.get(idx as usize).cloned() {
                if entry.is_dir {
                    state.prefix = entry.full_key;
                    rebuild_visible_entries(&mut state);
                    if let Some(app) = app_weak.upgrade() {
                        update_view(&app, &state);
                    }
                }
            }
        });
    }

    {
        let app_weak = app_weak.clone();
        let browser_state = browser_state.clone();
        app.on_navigate_up(move || {
            let mut state = browser_state.borrow_mut();
            state.prefix = parent_prefix(&state.prefix);
            rebuild_visible_entries(&mut state);
            if let Some(app) = app_weak.upgrade() {
                update_view(&app, &state);
            }
        });
    }

    {
        let app_weak = app_weak.clone();
        let browser_state = browser_state.clone();
        app.on_search_changed(move |value: SharedString| {
            let mut state = browser_state.borrow_mut();
            state.search = value.to_string();
            rebuild_visible_entries(&mut state);
            if let Some(app) = app_weak.upgrade() {
                update_view(&app, &state);
            }
        });
    }

    {
        let app_weak = app_weak.clone();
        let browser_state = browser_state.clone();
        app.on_refresh_clicked(move || {
            let app_weak = app_weak.clone();
            let browser_state = browser_state.clone();
            slint::spawn_local(async move {
                if let Err(err) = refresh_objects(browser_state, app_weak.clone()).await {
                    if let Some(app) = app_weak.upgrade() {
                        app.set_is_loading(false);
                        app.set_drive_status(format!("Refresh failed: {err}").into());
                    }
                }
            })
            .unwrap();
        });
    }

    {
        let app_weak = app_weak.clone();
        app.on_mount_drive_clicked(move || {
            let app_weak = app_weak.clone();
            if let Some(app) = app_weak.upgrade() {
                app.set_is_loading(true);
                app.set_drive_status("Mounting CloudDrive...".into());
            }

              slint::spawn_local(async move {
                  let message = match mount_virtual_drive().await {
                      Ok(message) => {
                          println!("[drive:mount] {message}");
                          message
                      }
                      Err(err) => {
                          log_drive_error("mount", &err);
                          format!("Drive mount failed: {err}")
                      }
                  };

                if let Some(app) = app_weak.upgrade() {
                    app.set_is_loading(false);
                    app.set_drive_mounted(virtual_drive::is_mounted(virtual_drive::preferred_drive_letter()));
                    app.set_drive_status(message.into());
                }
            })
            .unwrap();
        });
    }

    {
        let app_weak = app_weak.clone();
        app.on_sync_drive_clicked(move || {
            let app_weak = app_weak.clone();
            if let Some(app) = app_weak.upgrade() {
                app.set_is_loading(true);
                app.set_drive_status("Syncing CloudDrive...".into());
            }

              slint::spawn_local(async move {
                  let message = match sync_virtual_drive_cache().await {
                      Ok(message) => {
                          let message = format!(
                              "{} | {}",
                              message,
                              virtual_drive::status_text(virtual_drive::preferred_drive_letter())
                          );
                          println!("[drive:sync] {message}");
                          message
                      }
                      Err(err) => {
                          log_drive_error("sync", &err);
                          format!("Drive sync failed: {err}")
                      }
                  };

                if let Some(app) = app_weak.upgrade() {
                    app.set_is_loading(false);
                    app.set_drive_mounted(virtual_drive::is_mounted(virtual_drive::preferred_drive_letter()));
                    app.set_drive_status(message.into());
                }
            })
            .unwrap();
        });
    }

    {
        let app_weak = app_weak.clone();
        app.on_unmount_drive_clicked(move || {
            let app_weak = app_weak.clone();
            if let Some(app) = app_weak.upgrade() {
                app.set_drive_status("Unmounting CloudDrive...".into());
            }

              let drive_letter = virtual_drive::preferred_drive_letter();
              let message = match virtual_drive::unmount_drive(drive_letter) {
                  Ok(()) => {
                      let message = virtual_drive::status_text(drive_letter);
                      println!("[drive:unmount] {message}");
                      message
                  }
                  Err(err) => {
                      log_drive_error("unmount", &err);
                      format!("Drive unmount failed: {err}")
                  }
              };
            refresh_drive_status(&app_weak);
            set_drive_status(&app_weak, message);
        });
    }

    {
        let app_weak = app_weak.clone();
        let browser_state = browser_state.clone();
        app.on_upload_clicked(move || {
            let selected_path = match rfd::FileDialog::new().pick_file() {
                Some(path) => path,
                None => return,
            };

            let prefix = {
                let state = browser_state.borrow();
                state.prefix.clone()
            };

            let file_name = match selected_path.file_name().and_then(|name| name.to_str()) {
                Some(name) => name.to_string(),
                None => return,
            };

            let target_key = format!("{prefix}{file_name}");
            let app_weak = app_weak.clone();
            let browser_state = browser_state.clone();

            if let Some(app) = app_weak.upgrade() {
                app.set_is_loading(true);
            }

            slint::spawn_local(async move {
                if let Err(err) = upload_selected_file(selected_path, target_key).await {
                    eprintln!("{err}");
                }
                if let Err(err) = refresh_objects(browser_state, app_weak.clone()).await {
                    eprintln!("{err}");
                    if let Some(app) = app_weak.upgrade() {
                        app.set_is_loading(false);
                        app.set_drive_status(format!("Refresh failed: {err}").into());
                    }
                }
            })
            .unwrap();
        });
    }

    {
        let app_weak = app_weak.clone();
        let browser_state = browser_state.clone();
        app.on_upload_folder_clicked(move || {
            let selected_folder = match rfd::FileDialog::new().pick_folder() {
                Some(path) => path,
                None => return,
            };

            let prefix = {
                let state = browser_state.borrow();
                state.prefix.clone()
            };

            let app_weak = app_weak.clone();
            let browser_state = browser_state.clone();

            if let Some(app) = app_weak.upgrade() {
                app.set_is_loading(true);
            }

            slint::spawn_local(async move {
                if let Err(err) = upload_folder(selected_folder, prefix).await {
                    eprintln!("{err}");
                }
                if let Err(err) = refresh_objects(browser_state, app_weak.clone()).await {
                    eprintln!("{err}");
                    if let Some(app) = app_weak.upgrade() {
                        app.set_is_loading(false);
                        app.set_drive_status(format!("Refresh failed: {err}").into());
                    }
                }
            })
            .unwrap();
        });
    }

    {
        let browser_state = browser_state.clone();
        app.on_download_file(move |idx| {
            let entry = {
                let state = browser_state.borrow();
                state.visible_entries.get(idx as usize).cloned()
            };

            let Some(entry) = entry else {
                return;
            };

            if entry.is_dir {
                return;
            }

            let suggested_name = default_file_name(&entry.full_key);
            let save_path = match rfd::FileDialog::new()
                .set_file_name(&suggested_name)
                .save_file()
            {
                Some(path) => path,
                None => return,
            };

            slint::spawn_local(async move {
                if let Err(err) = download_to_path(entry.full_key, save_path).await {
                    eprintln!("{err}");
                }
            })
            .unwrap();
        });
    }

    {
        let app_weak = app_weak.clone();
        let browser_state = browser_state.clone();
        app.on_delete_file(move |idx| {
            let entry = {
                let state = browser_state.borrow();
                state.visible_entries.get(idx as usize).cloned()
            };

            let Some(entry) = entry else {
                return;
            };

            if entry.is_dir {
                return;
            }

            if let Some(app) = app_weak.upgrade() {
                app.set_is_loading(true);
            }

            let app_weak = app_weak.clone();
            let browser_state = browser_state.clone();
            slint::spawn_local(async move {
                if let Err(err) = delete_object(entry.full_key).await {
                    eprintln!("{err}");
                }
                if let Err(err) = refresh_objects(browser_state, app_weak.clone()).await {
                    eprintln!("{err}");
                    if let Some(app) = app_weak.upgrade() {
                        app.set_is_loading(false);
                        app.set_drive_status(format!("Refresh failed: {err}").into());
                    }
                }
            })
            .unwrap();
        });
    }

    app.run().unwrap();
}
