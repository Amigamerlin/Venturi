//! What a drop from the file manager brings in: its files, and its folders
//! walked into a tree of the media they hold.

use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq)]
pub struct MediaTree {
    /// Name and parent (an index into `folders`), each after its parent.
    pub folders: Vec<(String, Option<usize>)>,
    /// Each with the folder it goes into.
    pub files: Vec<(PathBuf, Option<usize>)>,
}

impl MediaTree {
    /// Files are taken as they are; folders keep only media files
    /// (`vv_media::is_media_path`) and the subfolders holding some.
    pub fn scan(paths: &[PathBuf]) -> Self {
        let mut tree = Self::default();
        for path in paths {
            if path.is_dir() {
                tree.walk(path, None);
            } else {
                tree.files.push((path.clone(), None));
            }
        }
        tree
    }

    /// `false`, leaving nothing behind, if `dir` holds no media.
    fn walk(&mut self, dir: &Path, parent: Option<usize>) -> bool {
        let (folders_before, files_before) = (self.folders.len(), self.files.len());
        let name = dir
            .file_name()
            .map_or_else(|| dir.to_string_lossy(), |n| n.to_string_lossy())
            .into_owned();
        let index = self.folders.len();
        self.folders.push((name, parent));
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        entries.sort_by_key(|e| e.file_name());
        let mut any = false;
        for entry in entries {
            let path = entry.path();
            // A symlinked folder could loop back on itself.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                any |= self.walk(&path, Some(index));
            } else if vv_media::is_media_path(&path) && path.is_file() {
                self.files.push((path, Some(index)));
                any = true;
            }
        }
        if !any {
            self.folders.truncate(folders_before);
            self.files.truncate(files_before);
        }
        any
    }
}

#[cfg(test)]
#[path = "tests/media_tree.rs"]
mod tests;
