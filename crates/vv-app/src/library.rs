//! User library shared by all projects, in `$XDG_DATA_HOME/venturi/library`:
//! assets (pool items, their files copied in) and attribute presets, one
//! JSON file each so they can be synced or handed around one by one, and the
//! folders the assets are sorted in.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};
use vv_core::clipboard::ClipboardPayload;
use vv_core::{Clip, MediaItem, Rational};

use crate::paste_attributes::{Attribute, KeyframeAnchor, TimelineFormat};
use crate::settings::Shortcut;

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct AssetFile {
    version: u32,
    name: String,
    #[serde(default)]
    folder: Option<String>,
    /// `ClipboardPayload::to_text`: it already carries compound clips with
    /// their nested timelines.
    payload: String,
}

#[derive(Clone)]
pub(crate) struct Asset {
    pub id: String,
    pub name: String,
    pub folder: Option<String>,
    pub payload: ClipboardPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct LibraryFolder {
    pub id: String,
    pub name: String,
    pub parent: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct FoldersFile {
    version: u32,
    folders: Vec<LibraryFolder>,
}

impl Asset {
    pub fn item(&self) -> Option<&MediaItem> {
        self.payload.media_items().next()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct AttributePreset {
    pub version: u32,
    pub name: String,
    pub attributes: Vec<Attribute>,
    pub anchor: KeyframeAnchor,
    /// Added after the target's filters instead of replacing them.
    #[serde(default)]
    pub append_filters: bool,
    /// Applies the preset to the selected clips; unique among presets.
    #[serde(default)]
    pub shortcut: Option<Shortcut>,
    pub fps: Rational,
    pub resolution: (u32, u32),
    pub video: Option<Clip>,
    pub audio: Option<Clip>,
}

impl AttributePreset {
    pub fn new(
        name: String,
        format: TimelineFormat,
        video: Option<Clip>,
        audio: Option<Clip>,
    ) -> Self {
        Self {
            version: VERSION,
            name,
            attributes: Vec::new(),
            anchor: KeyframeAnchor::Start,
            append_filters: false,
            shortcut: None,
            fps: format.fps,
            resolution: format.resolution,
            video,
            audio,
        }
    }

    pub fn format(&self) -> TimelineFormat {
        TimelineFormat {
            fps: self.fps,
            resolution: self.resolution,
        }
    }
}

/// An asset whose files are still being copied.
struct PendingAsset {
    name: String,
    done: mpsc::Receiver<Result<Asset, String>>,
}

#[derive(Default)]
pub(crate) struct Library {
    /// `None`: no home directory, the library is in memory only.
    root: Option<PathBuf>,
    pub assets: Vec<Asset>,
    pub folders: Vec<LibraryFolder>,
    pub presets: Vec<(String, AttributePreset)>,
    pending: Vec<PendingAsset>,
}

impl Library {
    pub fn default_root() -> Option<PathBuf> {
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
            })?;
        Some(data.join("venturi").join("library"))
    }

    /// Unreadable entries are skipped with a message, never fatal.
    pub fn open(root: PathBuf) -> Self {
        let mut assets: Vec<Asset> = json_files(&root.join("assets"))
            .filter_map(|(id, text)| {
                let file: AssetFile = parse(&id, &text)?;
                let payload = match ClipboardPayload::from_text(&file.payload) {
                    Some(Ok(payload)) => payload,
                    _ => {
                        eprintln!("library asset {id}: unreadable payload");
                        return None;
                    }
                };
                Some(Asset {
                    id,
                    name: file.name,
                    folder: file.folder,
                    payload,
                })
            })
            .collect();
        assets.sort_by_cached_key(|a| a.name.to_lowercase());
        let folders = std::fs::read_to_string(root.join("folders.json"))
            .ok()
            .and_then(|text| parse::<FoldersFile>("folders", &text))
            .map_or_else(Vec::new, |f| f.folders);
        let mut presets: Vec<(String, AttributePreset)> = json_files(&root.join("presets"))
            .filter_map(|(id, text)| Some((id.clone(), parse(&id, &text)?)))
            .collect();
        presets.sort_by_cached_key(|(_, p)| p.name.to_lowercase());
        Self {
            root: Some(root),
            assets,
            folders,
            presets,
            pending: Vec::new(),
        }
    }

    /// Names of the assets still copying their files.
    pub fn pending(&self) -> impl Iterator<Item = &str> {
        self.pending.iter().map(|p| p.name.as_str())
    }

    /// Copies the files of `payload` into the library in the background.
    pub fn add_asset(&mut self, name: String, folder: Option<String>, payload: ClipboardPayload) {
        let (tx, done) = mpsc::channel();
        let root = self.root.clone();
        let asset = Asset {
            id: new_id(),
            name: name.clone(),
            folder,
            payload,
        };
        std::thread::spawn(move || {
            let result = match root {
                Some(root) => store_asset(&root, asset),
                None => Ok(asset),
            };
            let _ = tx.send(result);
        });
        self.pending.push(PendingAsset { name, done });
    }

    /// Collects the finished copies; returns the errors.
    pub fn poll(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            match self.pending[i].done.try_recv() {
                Ok(result) => {
                    let pending = self.pending.remove(i);
                    match result {
                        Ok(asset) => self.insert_asset(asset),
                        Err(e) => errors.push(format!("{}: {e}", pending.name)),
                    }
                }
                Err(mpsc::TryRecvError::Empty) => i += 1,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending.remove(i);
                }
            }
        }
        errors
    }

    fn insert_asset(&mut self, asset: Asset) {
        let at = self
            .assets
            .partition_point(|a| a.name.to_lowercase() <= asset.name.to_lowercase());
        self.assets.insert(at, asset);
    }

    pub fn rename_asset(&mut self, id: &str, name: String) -> Result<(), String> {
        let Some(index) = self.assets.iter().position(|a| a.id == id) else {
            return Ok(());
        };
        let mut asset = self.assets.remove(index);
        asset.name = name;
        let result = self.save_asset(&asset);
        self.insert_asset(asset);
        result
    }

    pub fn move_assets(&mut self, ids: &[String], folder: Option<String>) -> Result<(), String> {
        for index in 0..self.assets.len() {
            if ids.contains(&self.assets[index].id) && self.assets[index].folder != folder {
                self.assets[index].folder = folder.clone();
                self.save_asset(&self.assets[index])?;
            }
        }
        Ok(())
    }

    fn save_asset(&self, asset: &Asset) -> Result<(), String> {
        match &self.root {
            Some(root) => write_asset(root, asset),
            None => Ok(()),
        }
    }

    pub fn folder(&self, id: &str) -> Option<&LibraryFolder> {
        self.folders.iter().find(|f| f.id == id)
    }

    pub fn add_folder(&mut self, name: String, parent: Option<String>) -> Result<String, String> {
        let id = new_id();
        self.folders.push(LibraryFolder {
            id: id.clone(),
            name,
            parent,
        });
        self.save_folders()?;
        Ok(id)
    }

    pub fn rename_folder(&mut self, id: &str, name: String) -> Result<(), String> {
        if let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) {
            folder.name = name;
        }
        self.save_folders()
    }

    /// The caller checks that `parent` is not inside `id`.
    pub fn move_folder(&mut self, id: &str, parent: Option<String>) -> Result<(), String> {
        if let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) {
            folder.parent = parent;
        }
        self.save_folders()
    }

    /// As in the project's pool, its content moves up one level.
    pub fn remove_folder(&mut self, id: &str) -> Result<(), String> {
        let Some(index) = self.folders.iter().position(|f| f.id == id) else {
            return Ok(());
        };
        let removed = self.folders.remove(index);
        for folder in &mut self.folders {
            if folder.parent.as_deref() == Some(id) {
                folder.parent = removed.parent.clone();
            }
        }
        let contained: Vec<String> = self
            .assets
            .iter()
            .filter(|a| a.folder.as_deref() == Some(id))
            .map(|a| a.id.clone())
            .collect();
        self.move_assets(&contained, removed.parent)?;
        self.save_folders()
    }

    fn save_folders(&self) -> Result<(), String> {
        match &self.root {
            Some(root) => write_json(
                &root.join("folders.json"),
                &FoldersFile {
                    version: VERSION,
                    folders: self.folders.clone(),
                },
            ),
            None => Ok(()),
        }
    }

    /// Also deletes the copied files no other asset uses.
    pub fn remove_asset(&mut self, id: &str) -> Result<(), String> {
        let Some(index) = self.assets.iter().position(|a| a.id == id) else {
            return Ok(());
        };
        let removed = self.assets.remove(index);
        let Some(root) = &self.root else {
            return Ok(());
        };
        remove_file(&root.join("assets").join(format!("{id}.json")))?;
        let media_dir = root.join("media");
        for item in removed.payload.files() {
            let used = self
                .assets
                .iter()
                .any(|a| a.payload.files().any(|f| f.path == item.path));
            if !used && item.path.starts_with(&media_dir) {
                remove_file(&item.path)?;
                if let Some(dir) = item.path.parent()
                    && dir != media_dir
                {
                    let _ = std::fs::remove_dir(dir);
                }
            }
        }
        Ok(())
    }

    pub fn preset(&self, id: &str) -> Option<&AttributePreset> {
        self.presets.iter().find(|(i, _)| i == id).map(|(_, p)| p)
    }

    /// Id of the preset whose shortcut was pressed.
    pub fn preset_pressed(&self, input: &egui::InputState) -> Option<String> {
        self.presets
            .iter()
            .find(|(_, p)| p.shortcut.is_some_and(|s| s.pressed(input)))
            .map(|(id, _)| id.clone())
    }

    pub fn add_preset(&mut self, preset: AttributePreset) -> Result<String, String> {
        let id = new_id();
        self.presets.push((id.clone(), preset.clone()));
        self.update_preset(&id, |p| *p = preset)?;
        Ok(id)
    }

    /// Changes a preset and saves it. A shortcut taken by this preset is
    /// taken away from the others.
    pub fn update_preset(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut AttributePreset),
    ) -> Result<(), String> {
        let Some(index) = self.presets.iter().position(|(i, _)| i == id) else {
            return Ok(());
        };
        change(&mut self.presets[index].1);
        let mut dirty = vec![index];
        if let Some(shortcut) = self.presets[index].1.shortcut {
            for (i, (_, other)) in self.presets.iter_mut().enumerate() {
                if i != index && other.shortcut == Some(shortcut) {
                    other.shortcut = None;
                    dirty.push(i);
                }
            }
        }
        let result = match &self.root {
            Some(root) => dirty.iter().try_for_each(|&i| {
                let (id, preset) = &self.presets[i];
                write_json(&root.join("presets").join(format!("{id}.json")), preset)
            }),
            None => Ok(()),
        };
        self.presets
            .sort_by_cached_key(|(_, p)| p.name.to_lowercase());
        result
    }

    pub fn remove_preset(&mut self, id: &str) -> Result<(), String> {
        self.presets.retain(|(i, _)| i != id);
        match &self.root {
            Some(root) => remove_file(&root.join("presets").join(format!("{id}.json"))),
            None => Ok(()),
        }
    }
}

/// Copies each file into `media/<content hash>/`, so two assets sharing a
/// file share the copy and the file keeps its name in the projects.
fn store_asset(root: &Path, mut asset: Asset) -> Result<Asset, String> {
    let media_dir = root.join("media");
    std::fs::create_dir_all(&media_dir).map_err(|e| e.to_string())?;
    for item in asset.payload.files_mut() {
        if item.path.starts_with(&media_dir) {
            continue;
        }
        let file_name = item
            .path
            .file_name()
            .ok_or_else(|| format!("{}: not a file", item.path.display()))?
            .to_string_lossy();
        let dir = media_dir.join(format!("{:016x}", item.content_hash));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let dest = dir.join(file_name.as_ref());
        if !dest.exists() {
            // Through a temporary name: a copy cut short must not look done.
            let partial = dest.with_extension("partial");
            std::fs::copy(&item.path, &partial)
                .map_err(|e| format!("{}: {e}", item.path.display()))?;
            std::fs::rename(&partial, &dest).map_err(|e| e.to_string())?;
        }
        item.path = dest;
    }
    write_asset(root, &asset)?;
    Ok(asset)
}

fn write_asset(root: &Path, asset: &Asset) -> Result<(), String> {
    write_json(
        &root.join("assets").join(format!("{}.json", asset.id)),
        &AssetFile {
            version: VERSION,
            name: asset.name.clone(),
            folder: asset.folder.clone(),
            payload: asset.payload.to_text(),
        },
    )
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

/// `(file stem, content)` of the `.json` files in `dir`.
fn json_files(dir: &Path) -> impl Iterator<Item = (String, String)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "json" {
                return None;
            }
            let id = path.file_stem()?.to_string_lossy().into_owned();
            Some((id, std::fs::read_to_string(&path).ok()?))
        })
}

/// Entries written by a newer Venturi are skipped, not misread.
fn parse<T: serde::de::DeserializeOwned>(id: &str, text: &str) -> Option<T> {
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    match serde_json::from_str::<Versioned>(text) {
        Ok(v) if v.version > VERSION => {
            eprintln!("library entry {id}: version {} is newer", v.version);
            return None;
        }
        Err(e) => {
            eprintln!("library entry {id}: {e}");
            return None;
        }
        Ok(_) => {}
    }
    serde_json::from_str(text)
        .map_err(|e| eprintln!("library entry {id}: {e}"))
        .ok()
}

fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
#[path = "tests/library.rs"]
mod tests;
