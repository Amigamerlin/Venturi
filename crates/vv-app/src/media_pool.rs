//! State of a pool browser (the project's media pool, the user library):
//! multiple selection with the same rules as the timeline (plain click,
//! ctrl+click to add/remove, shift+click for a range, rubber-band selection
//! by dragging from the background), folders, rename, search. The logic
//! lives here as pure functions (testable without `egui::Ui`); the drawing
//! is in `pool_view.rs`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use vv_core::{FolderId, MediaId};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

/// Ids of the items and of the folders of a pool.
pub trait PoolId: Clone + Ord + Hash + std::fmt::Debug + Send + Sync + 'static {}
impl<T: Clone + Ord + Hash + std::fmt::Debug + Send + Sync + 'static> PoolId for T {}

pub type MediaPoolState = PoolState<MediaId, FolderId>;

pub struct PoolState<I, F> {
    pub selected: BTreeSet<I>,
    /// Exclusive with `selected`.
    pub selected_folder: Option<F>,
    /// Range origin for the next shift+click (which does *not* move it,
    /// as in `timeline_ui::TimelineState::selection_anchor`).
    anchor: Option<I>,
    /// Selection rectangle in progress, in screen coordinates.
    pub marquee: Option<(egui::Pos2, egui::Pos2)>,
    /// The media pool got the last click: Del/Backspace deletes the selected
    /// media instead of the clips on the timeline.
    pub focused: bool,
    pub sort: Sort,
    pub renaming: Option<Rename<I, F>>,
    /// A click on the name of the only selected item, with its time and
    /// name: it becomes a rename unless a double click follows (the "slow
    /// double click").
    pub rename_pending: Option<(I, String, f64)>,
    pub expanded: HashSet<F>,
    pub search: String,
    /// Scrolled into view the next time the pool is drawn.
    pub reveal: Option<I>,
    /// Residual touchpad inertia (px/s), see `timeline_ui::apply_kinetic_scroll`.
    pub scroll_vel: f32,
    /// The pool's ScrollArea as of the last frame: its offset is driven
    /// before `show`, when the current frame's geometry is not known yet.
    pub scroll_area: Option<PoolScrollArea>,
    /// Whether each media's file was missing, and at which path it was
    /// checked: only a new media or a changed path hits the disk again.
    offline: HashMap<I, (PathBuf, bool)>,
    /// Items dragged out of this pool: a drop on one of its folders moves
    /// them there.
    pub dragging: Vec<I>,
    /// Set by dragging the header's divider; `None`: all the room the
    /// duration leaves.
    pub name_col_w: Option<f32>,
    /// Screen x where the name column ends, as the header last laid it out:
    /// the rows line up with it.
    pub name_col_right: f32,
}

impl<I, F> Default for PoolState<I, F> {
    fn default() -> Self {
        Self {
            selected: BTreeSet::new(),
            selected_folder: None,
            anchor: None,
            marquee: None,
            focused: false,
            sort: Sort::default(),
            renaming: None,
            rename_pending: None,
            expanded: HashSet::new(),
            search: String::new(),
            reveal: None,
            scroll_vel: 0.0,
            scroll_area: None,
            offline: HashMap::new(),
            dragging: Vec::new(),
            name_col_w: None,
            name_col_right: f32::INFINITY,
        }
    }
}

#[derive(Clone, Copy)]
pub struct PoolScrollArea {
    pub id: egui::Id,
    pub viewport: egui::Rect,
    pub max_offset: f32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RenameTarget<I, F> {
    Item(I),
    Folder(F),
}

pub struct Rename<I, F> {
    pub target: RenameTarget<I, F>,
    pub text: String,
    /// Set until the text field has been shown once: it then takes focus
    /// with the whole name selected.
    pub just_started: bool,
}

impl MediaPoolState {
    /// Drops what refers to media or folders no longer in `project`.
    pub fn forget_removed(&mut self, project: &vv_core::Project) {
        self.forget(
            |&id| project.media_pool.contains_key(id),
            |&id| project.folders.contains_key(id),
        );
    }

    pub fn refresh_offline(&mut self, project: &vv_core::Project) {
        let files = project
            .media_pool
            .iter()
            .filter(|(_, item)| item.compound.is_none())
            .map(|(id, item)| (id, item.path.as_path()));
        self.refresh_offline_files(files);
    }
}

impl<I: PoolId, F: PoolId> PoolState<I, F> {
    /// Drops what refers to items or folders that no longer exist.
    pub fn forget(&mut self, item_exists: impl Fn(&I) -> bool, folder_exists: impl Fn(&F) -> bool) {
        self.selected.retain(&item_exists);
        self.anchor = self.anchor.take().filter(&item_exists);
        self.rename_pending = self
            .rename_pending
            .take()
            .filter(|(id, ..)| item_exists(id));
        self.dragging.retain(&item_exists);
        if self.renaming.as_ref().is_some_and(|r| match &r.target {
            RenameTarget::Item(id) => !item_exists(id),
            RenameTarget::Folder(id) => !folder_exists(id),
        }) {
            self.renaming = None;
        }
        self.expanded.retain(&folder_exists);
        self.selected_folder = self.selected_folder.take().filter(&folder_exists);
    }

    /// `order` is the order the items are drawn in the panel: that is what
    /// defines the range of a shift+click.
    pub fn click(&mut self, clicked: I, modifiers: egui::Modifiers, order: &[I]) {
        let (selected, anchor) = apply_click(
            &self.selected,
            self.anchor.take(),
            clicked,
            modifiers,
            order,
        );
        self.selected = selected;
        self.anchor = anchor;
        self.selected_folder = None;
    }

    /// Replaces the selection with `hits`, in drawing order: the first
    /// becomes the anchor for a possible later shift+click.
    pub fn select_only(&mut self, hits: impl IntoIterator<Item = I>) {
        let hits: Vec<I> = hits.into_iter().collect();
        self.anchor = hits.first().cloned();
        self.selected = hits.into_iter().collect();
        self.selected_folder = None;
    }

    /// Click on a column header: sorts by that criterion, or flips the
    /// direction if it was already the active one.
    pub fn toggle_sort(&mut self, key: SortKey) {
        if self.sort.key == key {
            self.sort.ascending = !self.sort.ascending;
        } else {
            self.sort = Sort {
                key,
                ascending: true,
            };
        }
    }

    pub fn select_folder(&mut self, folder: F) {
        self.clear();
        self.selected_folder = Some(folder);
    }

    /// The only selected item, if exactly one is.
    pub fn single_selected(&self) -> Option<&I> {
        let mut selected = self.selected.iter();
        match (selected.next(), selected.next()) {
            (Some(id), None) => Some(id),
            _ => None,
        }
    }

    pub fn start_rename(&mut self, target: RenameTarget<I, F>, name: String) {
        self.rename_pending = None;
        self.renaming = Some(Rename {
            target,
            text: name,
            just_started: true,
        });
    }

    /// Selects `item` and makes it visible: opens the folders containing it
    /// (`parent_of` gives the parent of a folder), drops a search hiding
    /// it, scrolls to it.
    pub fn reveal(
        &mut self,
        item: I,
        name: &str,
        folder: Option<F>,
        parent_of: impl Fn(&F) -> Option<Option<F>>,
    ) {
        if !matches_search(name, &self.search) {
            self.search.clear();
        }
        let mut visited = HashSet::new();
        let mut folder = folder;
        while let Some(f) = folder
            && let Some(parent) = parent_of(&f)
            && visited.insert(f.clone())
        {
            self.expanded.insert(f);
            folder = parent;
        }
        self.select_only([item.clone()]);
        self.rename_pending = None;
        self.scroll_vel = 0.0;
        self.reveal = Some(item);
    }

    pub fn is_offline(&self, id: &I) -> bool {
        self.offline.get(id).is_some_and(|&(_, missing)| missing)
    }

    /// Only a new item or a changed path hits the disk again.
    pub fn refresh_offline_files<'a>(&mut self, files: impl Iterator<Item = (I, &'a Path)>) {
        let files: Vec<(I, &Path)> = files.collect();
        self.offline
            .retain(|id, _| files.iter().any(|(f, _)| f == id));
        for (id, path) in files {
            if self.offline.get(&id).is_some_and(|(p, _)| p == path) {
                continue;
            }
            self.offline.insert(id, (path.to_owned(), !path.exists()));
        }
    }

    /// Makes the next `refresh_offline` check every file again.
    pub fn invalidate_offline(&mut self) {
        self.offline.clear();
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.selected_folder = None;
    }
}

/// Sorts the panel items according to `sort`. On equal duration the order
/// stays the one by name, so the list does not jump on every redraw.
pub fn sort_items<T>(
    items: &mut [T],
    sort: Sort,
    name: impl Fn(&T) -> &str,
    duration_secs: impl Fn(&T) -> f64,
) {
    items.sort_by(|a, b| {
        let by_name = natural_cmp(name(a), name(b));
        let ord = match sort.key {
            SortKey::Name => by_name,
            SortKey::Duration => duration_secs(a)
                .partial_cmp(&duration_secs(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(by_name),
        };
        if sort.ascending { ord } else { ord.reverse() }
    });
}

/// Drag payload of a folder row.
pub struct FolderDrag<F>(pub F);

pub enum PoolRow<T, F> {
    Folder { id: F, depth: usize },
    Item { item: T, depth: usize },
}

/// The pool as drawn: at each level the folders (by name) and then the
/// items, in the order given; the content of collapsed folders is left out.
/// A missing parent folder counts as the root.
pub fn tree_rows<T, F: PoolId>(
    folders: &[(F, &str, Option<F>)],
    items: Vec<(T, Option<F>)>,
    expanded: &HashSet<F>,
) -> Vec<PoolRow<T, F>> {
    build_rows(folders, items, expanded, None)
}

/// The rows while searching: `items` are the matches, and only the folders
/// leading to one of them are shown, all expanded.
pub fn search_rows<T, F: PoolId>(
    folders: &[(F, &str, Option<F>)],
    items: Vec<(T, Option<F>)>,
) -> Vec<PoolRow<T, F>> {
    let parent_of = |folder: &F| {
        folders
            .iter()
            .find(|(id, ..)| id == folder)
            .map(|(_, _, parent)| parent.clone())
    };
    let mut open = HashSet::new();
    for (_, folder) in &items {
        let mut current = folder.clone();
        while let Some(folder) = current
            && let Some(parent) = parent_of(&folder)
            && open.insert(folder)
        {
            current = parent;
        }
    }
    build_rows(folders, items, &open, Some(&open))
}

/// Whether `folder` can go into `target`: not into itself nor into one of
/// its descendants.
pub fn can_move_folder<F: PoolId>(
    folders: &[(F, &str, Option<F>)],
    folder: &F,
    target: Option<&F>,
) -> bool {
    let mut visited = HashSet::new();
    let mut current = target.cloned();
    while let Some(f) = current {
        if f == *folder || !visited.insert(f.clone()) {
            return false;
        }
        current = folders
            .iter()
            .find(|(id, ..)| *id == f)
            .and_then(|(_, _, parent)| parent.clone());
    }
    true
}

/// Case-insensitive substring match; an empty query matches everything.
pub fn matches_search(name: &str, query: &str) -> bool {
    name.to_lowercase().contains(&query.trim().to_lowercase())
}

fn build_rows<T, F: PoolId>(
    folders: &[(F, &str, Option<F>)],
    items: Vec<(T, Option<F>)>,
    expanded: &HashSet<F>,
    visible: Option<&HashSet<F>>,
) -> Vec<PoolRow<T, F>> {
    let exists = |folder: Option<F>| folder.filter(|f| folders.iter().any(|(id, ..)| id == f));
    let mut sorted: Vec<_> = folders.to_vec();
    sorted.sort_by(|a, b| natural_cmp(a.1, b.1));
    let mut items: Vec<(Option<T>, Option<F>)> = items
        .into_iter()
        .map(|(item, folder)| (Some(item), exists(folder)))
        .collect();
    let mut rows = Vec::new();
    let mut visited = HashSet::new();
    push_level(
        None,
        0,
        &sorted,
        &mut items,
        expanded,
        visible,
        &mut visited,
        &mut rows,
        &exists,
    );
    rows
}

#[allow(clippy::too_many_arguments)]
fn push_level<T, F: PoolId>(
    parent: Option<F>,
    depth: usize,
    folders: &[(F, &str, Option<F>)],
    items: &mut [(Option<T>, Option<F>)],
    expanded: &HashSet<F>,
    visible: Option<&HashSet<F>>,
    visited: &mut HashSet<F>,
    rows: &mut Vec<PoolRow<T, F>>,
    exists: &dyn Fn(Option<F>) -> Option<F>,
) {
    for (id, _, folder_parent) in folders {
        if visible.is_some_and(|v| !v.contains(id)) {
            continue;
        }
        // `visited`: a corrupted file with a parent cycle must not recurse forever.
        if exists(folder_parent.clone()) != parent || !visited.insert(id.clone()) {
            continue;
        }
        rows.push(PoolRow::Folder {
            id: id.clone(),
            depth,
        });
        if expanded.contains(id) {
            push_level(
                Some(id.clone()),
                depth + 1,
                folders,
                items,
                expanded,
                visible,
                visited,
                rows,
                exists,
            );
        }
    }
    for (item, folder) in items.iter_mut() {
        if *folder == parent
            && let Some(item) = item.take()
        {
            rows.push(PoolRow::Item { item, depth });
        }
    }
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

fn apply_click<I: PoolId>(
    current: &BTreeSet<I>,
    anchor: Option<I>,
    clicked: I,
    modifiers: egui::Modifiers,
    order: &[I],
) -> (BTreeSet<I>, Option<I>) {
    if modifiers.shift {
        let range = anchor
            .as_ref()
            .and_then(|a| {
                let from = order.iter().position(|id| id == a)?;
                let to = order.iter().position(|id| *id == clicked)?;
                Some(order[from.min(to)..=from.max(to)].iter().cloned().collect())
            })
            .unwrap_or_else(|| BTreeSet::from([clicked.clone()]));
        (range, anchor.or(Some(clicked)))
    } else if modifiers.command {
        let mut set = current.clone();
        if !set.remove(&clicked) {
            set.insert(clicked.clone());
        }
        (set, Some(clicked))
    } else {
        (BTreeSet::from([clicked.clone()]), Some(clicked))
    }
}

#[cfg(test)]
#[path = "tests/media_pool.rs"]
mod tests;
