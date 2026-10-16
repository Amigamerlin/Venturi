use super::*;
use vv_core::clipboard::ClipboardOrigin;
use vv_core::{MediaId, MediaMeta, Project};

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vv-app-library-test-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn project_with_file(path: &Path, hash: u64) -> (Project, MediaId) {
    std::fs::write(path, b"not really a video").unwrap();
    let mut project = Project::default();
    let id = project.media_pool.insert(MediaItem {
        path: path.to_owned(),
        meta: MediaMeta {
            duration_frames: 10,
            fps: Rational::new(30, 1),
            width: 16,
            height: 16,
            has_video: true,
            has_audio: false,
            sample_rate: 0,
            channels: 0,
            audio_streams: 0,
            file: Default::default(),
        },
        content_hash: hash,
        compound: None,
        folder: None,
    });
    (project, id)
}

fn payload(project: &Project, id: MediaId) -> ClipboardPayload {
    let origin = ClipboardOrigin {
        instance: 0,
        epoch: 0,
    };
    ClipboardPayload::new(origin, project, &[id], &[])
}

#[test]
fn an_asset_copies_its_file_and_survives_a_reopen() {
    let root = temp_root("asset");
    let original = root.join("clip.mp4");
    let (project, id) = project_with_file(&original, 0xab);

    let asset = store_asset(&root, "a".into(), "Intro".into(), payload(&project, id)).unwrap();

    let copied = &asset.item().unwrap().path;
    assert!(copied.starts_with(root.join("media")));
    assert_eq!(std::fs::read(copied).unwrap(), b"not really a video");
    std::fs::remove_file(&original).unwrap();
    let library = Library::open(root);
    assert_eq!(library.assets.len(), 1);
    assert_eq!(library.assets[0].name, "Intro");
    assert_eq!(&library.assets[0].item().unwrap().path, copied);
}

#[test]
fn removing_an_asset_keeps_the_files_other_assets_use() {
    let root = temp_root("remove");
    let (project, id) = project_with_file(&root.join("clip.mp4"), 0xcd);
    store_asset(&root, "a".into(), "A".into(), payload(&project, id)).unwrap();
    store_asset(&root, "b".into(), "B".into(), payload(&project, id)).unwrap();
    let mut library = Library::open(root.clone());
    let copied = library.assets[0].item().unwrap().path.clone();

    library.remove_asset("a").unwrap();
    assert!(copied.exists(), "still used by B");
    library.remove_asset("b").unwrap();
    assert!(!copied.exists());
    assert!(Library::open(root).assets.is_empty());
}

fn preset(name: &str, hotkey: Option<u8>) -> AttributePreset {
    let format = TimelineFormat {
        fps: Rational::new(30, 1),
        resolution: (1920, 1080),
    };
    let mut preset = AttributePreset::new(name.into(), format, None, None);
    preset.attributes = vec![Attribute::Zoom];
    preset.hotkey = hotkey;
    preset
}

#[test]
fn a_hotkey_belongs_to_one_preset_only() {
    let root = temp_root("hotkey");
    let mut library = Library::open(root.clone());
    library.add_preset(preset("First", Some(3))).unwrap();
    library.add_preset(preset("Second", Some(3))).unwrap();

    let library = Library::open(root);
    assert_eq!(library.presets.len(), 2);
    assert_eq!(library.preset_for_hotkey(3).unwrap().name, "Second");
    assert_eq!(
        library
            .presets
            .iter()
            .filter(|(_, p)| p.hotkey.is_some())
            .count(),
        1
    );
}

#[test]
fn entries_of_a_newer_version_are_skipped() {
    let root = temp_root("version");
    let mut library = Library::open(root.clone());
    let id = library.add_preset(preset("Old", None)).unwrap();
    let path = root.join("presets").join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"version\": 1", "\"version\": 99");
    std::fs::write(&path, text).unwrap();

    assert!(Library::open(root).presets.is_empty());
}
