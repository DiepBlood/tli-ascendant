//! The app's settings, kept as JSON in settings.json in [`app_dir`]
//! (%APPDATA%\Ascendant unless [`home`] names another folder).

use crate::engine::data::Tool;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Reads the game now; such as "F9" or "Ctrl+Shift+R".
    pub read_hotkey: String,
    /// Turns auto read on or off.
    pub auto_hotkey: String,
    /// Click-through: if it was on when Ascendant was last closed, it starts on next time. It is turned on and off
    /// by the navigation rail's "Thru" light, whose clicks stay in the window; the click-through hotkey of 1.0.1
    /// and before is gone, and an old file's `through_hotkey` is ignored.
    pub click_through: bool,
    /// How opaque the window is, from [`MIN_OPACITY`] to 1.
    pub opacity: f32,
    /// A part of the game window's title.
    pub window_title: String,
    pub rollouts: usize,
    /// Held instrument keys.
    pub tools: Vec<String>,
    /// Days until Riot when `tools` were saved: a higher count read twice
    /// at start is a new run and clears them.
    pub tools_days: Option<u32>,
    /// The run's theme, an id of the game data's themes; None for a run without one.
    pub theme: Option<u32>,
    /// Read the game by itself when its screen changes and settles.
    pub auto_read: bool,
    /// Move the window over the empty area under the game's instrument panel
    /// when the game is first found.
    pub auto_place: bool,
    /// Where the player last moved the window: its outer top left in
    /// physical screen pixels. Used instead of `auto_place` while it is on a
    /// monitor; None until the player moves the window.
    pub window_pos: Option<[i32; 2]>,
    /// The window's inner size in points the player last resized it to,
    /// which it opens at; None until the player resizes it.
    pub window_size: Option<[f32; 2]>,
    /// Where the player last moved each small window that opens as a window of its own ("What's new", the run check,
    /// the instruments and theme; 1.1.0), by its key (see `Float::key` in ui.rs): its outer top left in physical
    /// screen pixels. A window not listed opens over the main window, where it floated before. An older settings file
    /// has none, and an entry that does not read (another type, a broken value) is left out alone, the rest of the
    /// file still read (see [`float_places`]); an older version reading a newer file ignores the field.
    #[serde(deserialize_with = "float_places")]
    pub float_pos: std::collections::BTreeMap<String, [i32; 2]>,
    /// Keep a log of each run's reads, advice and corrections, with the
    /// frames read, in %APPDATA%\Ascendant\runs, see [`super::runlog`].
    pub run_log: bool,
    /// Keep every run log and frame (Tartarus `runs` and Vorax `vorax`), never deleting any. Off for a new player
    /// ([`Settings::default`]): only the newest 10 finished games of each are kept, older folders are deleted (see
    /// `runlog::prune`). A settings file without this field gets on: every save writes it, so only a file an older
    /// version saved lacks it (an older version drops the fields it does not know), and that player's logs must not
    /// be deleted. Applied through [`super::runlog::set_keep_all`].
    #[serde(default = "keep_all_when_missing")]
    pub keep_all_logs: bool,
    /// Observe mode: the run log also keeps about 25 game frames a second
    /// while the prison shows,
    /// in its folder's `observe` subfolder, to review how a run looked as it
    /// was played (see [`super::runlog::RunLog::observed`]). Off by default.
    pub observe: bool,
    /// Ask GitHub for a newer release at start and then every hour while Ascendant runs (on by default, see
    /// `update::CHECK_EVERY`). Only the check runs by itself: an update is downloaded, verified and swapped in only
    /// when the player presses "Update" (see `update.rs`).
    pub check_updates: bool,
    /// The version ("X.Y.Z") whose "Updated to" notice the player dismissed, with its "x" or by playing a game since
    /// (see `update::show_updated`); empty until the first.
    pub updated_seen: String,
    /// Offer to install when running from outside the install folder.
    pub offer_install: bool,
    /// Send each finished run's log to warden.arcasti.dev, see
    /// [`super::share`]; None until the player is asked.
    pub share_logs: Option<bool>,
    /// When the player last turned sharing on, as a run folder name (UTC):
    /// only runs whose folder name sorts at or after it are sent.
    pub shared_from: String,
    /// A random lowercase UUID sent with each run log; empty until the
    /// player first chooses to share.
    pub install_id: String,
    /// The desktop shortcut was made once, at install or at the first start
    /// of an install made before it existed; a shortcut the player deletes is
    /// not made again.
    pub desktop_shortcut: bool,
    /// Mark on the game what to click for the advised move, see
    /// [`super::mark`].
    pub highlight: bool,
    /// Show on the game, just above the advised target, the advised move's own expected value (Tartarus: the
    /// expected loot, Vorax: the expected final activity), the same number the window shows. Separate from
    /// `highlight`: on with the mark off it still shows. On by default and in older files (the player's choice, 2026-11-07).
    pub show_value: bool,
    /// The game client's language, which the reader reads and the window
    /// writes in: "en" or "zh-TW" (see [`crate::lang::Lang::key`]).
    pub language: String,
    /// Live advice for Vorax: switches over once the game log says Vorax was entered, see
    /// [`super::vorax_session`].
    pub vorax_enabled: bool,
    /// Number of rollouts per option in Vorax (see `VORAX_ROLLOUTS` in ascendant-cli).
    pub vorax_rollouts: u32,
    /// Vorax observe mode: saves frames when an unknown card, icon or tooltip is read, so the name table and
    /// templates can be filled in later. On by default: it writes only when something unknown is read, and those
    /// crops are what "Export this run" packs, so a player can report a name the table does not know yet.
    pub vorax_learn: bool,
    /// Saves the tank label strip after the Vorax resolve sound, to study the rules. Off by default (a developer aid;
    /// existing settings files keep their value).
    pub vorax_strips: bool,
    /// Font size: overall scale, 1.0 standard, 1.15 large, 1.3 extra large (see `text_scale` in ui.rs). Old settings
    /// files without this field get standard.
    pub font_scale: f32,
    /// Layout: "auto" (switches between full and compact by window size), "full", "compact" (see `next_compact` in
    /// ui.rs). Old settings files without this field get auto.
    pub layout: String,
    /// The game log (UE_game.log) chosen on the settings page; None finds it by itself (see
    /// [`super::gamelog::locate`]). Changing it rebuilds the auto read watcher.
    pub game_log: Option<PathBuf>,
    /// "Start as administrator": at start Ascendant starts itself again as administrator, when it runs without (see
    /// [`super::elevation::should_relaunch`]); for a game run as administrator, whose window otherwise keeps the
    /// hotkeys from Ascendant. Off by default and in older files: Windows asks at every start.
    pub start_as_admin: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            read_hotkey: "F9".to_string(),
            auto_hotkey: "F8".to_string(),
            click_through: false,
            opacity: 1.0,
            window_title: "Torchlight".to_string(),
            rollouts: 1600,
            tools: Vec::new(),
            tools_days: None,
            theme: None,
            auto_read: true,
            auto_place: true,
            window_pos: None,
            window_size: None,
            float_pos: std::collections::BTreeMap::new(),
            run_log: true,
            keep_all_logs: false,
            observe: false,
            check_updates: true,
            updated_seen: String::new(),
            offer_install: true,
            share_logs: None,
            shared_from: String::new(),
            install_id: String::new(),
            desktop_shortcut: false,
            highlight: false,
            show_value: true,
            language: "en".to_string(),
            vorax_enabled: true,
            vorax_rollouts: 4096,
            vorax_learn: true,
            vorax_strips: false,
            font_scale: 1.0,
            layout: "auto".to_string(),
            game_log: None,
            start_as_admin: false,
        }
    }
}

/// [`Settings::keep_all_logs`] for a settings file without the field: on, so nothing is deleted.
fn keep_all_when_missing() -> bool {
    true
}

/// [`Settings::float_pos`] as saved: each entry that reads as two whole numbers, the rest left out, and nothing at all
/// for a value that is not a map. One broken entry so loses only itself: a field that failed to read would fail the
/// whole file, and the defaults would come back for every setting.
fn float_places<'de, D: serde::Deserializer<'de>>(d: D) -> Result<std::collections::BTreeMap<String, [i32; 2]>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    let serde_json::Value::Object(map) = value else { return Ok(std::collections::BTreeMap::new()) };
    Ok(map.into_iter().filter_map(|(k, v)| serde_json::from_value::<[i32; 2]>(v).ok().map(|at| (k, at))).collect())
}

/// The least opacity the window can be set to: below it the window is
/// lost against the game.
pub const MIN_OPACITY: f32 = 0.5;

/// The file in %APPDATA%\Ascendant that names another home for the app's files.
pub const HOME_FILE: &str = "home.txt";

/// Name of the settings folder under %APPDATA%.
pub const DIR_NAME: &str = "Ascendant";

/// Name of the settings folder before the rename (%APPDATA%\warden). Old versions and restore points still use it, so
/// the new version only reads it and copies home.txt; it never moves or deletes anything.
pub const OLD_DIR_NAME: &str = "warden";

/// %APPDATA%; None when it is not set.
fn appdata() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

/// The folder [`HOME_FILE`] names, read once: the app's files and its
/// install go there (a 2026-10-31 player moved 2.5 GB of run logs off a
/// nearly full C: to a faster drive). None without one.
pub fn home() -> Option<PathBuf> {
    static HOME: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    HOME.get_or_init(|| home_in(&appdata()?)).clone()
}

/// The folder that home.txt under `appdata` points to: first Ascendant\home.txt; if missing, the pre-rename
/// warden\home.txt, copied to Ascendant\ (the old file stays, so a rollback to the old version still finds it). If
/// the copy fails the old file's content is still used. If the old file is not an absolute path (old versions ignore
/// it too) it is not copied: creating an empty Ascendant folder would make [`dir_in`] use it instead, and the old
/// folder's settings would seem to vanish.
fn home_in(appdata: &Path) -> Option<PathBuf> {
    let parse = |text: &str| Some(PathBuf::from(text.trim())).filter(|dir| dir.is_absolute());
    let new = appdata.join(DIR_NAME).join(HOME_FILE);
    if let Ok(text) = std::fs::read_to_string(&new) {
        return parse(&text);
    }
    let text = std::fs::read_to_string(appdata.join(OLD_DIR_NAME).join(HOME_FILE)).ok()?;
    let dir = parse(&text)?;
    if std::fs::create_dir_all(appdata.join(DIR_NAME)).is_ok() {
        let _ = std::fs::write(&new, &text);
    }
    Some(dir)
}

/// Where the app keeps its files: [`home`], else %APPDATA%\Ascendant; None
/// when APPDATA is not set.
pub fn app_dir() -> Option<PathBuf> {
    home().or_else(|| appdata().map(|d| dir_in(&d)))
}

/// Folder when there is no home.txt: %APPDATA%\Ascendant. If it does not exist yet and the pre-rename
/// %APPDATA%\warden does, keep using the old one: settings and run logs can be several GB, so nothing is moved.
fn dir_in(appdata: &Path) -> PathBuf {
    let (new, old) = (appdata.join(DIR_NAME), appdata.join(OLD_DIR_NAME));
    if !new.exists() && old.is_dir() { old } else { new }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        app_dir().map(|d| d.join("settings.json"))
    }

    /// The saved settings, or the defaults when there are none, see [`Settings::load_from`].
    pub fn load() -> Settings {
        Settings::path().map_or_else(Settings::default, |p| Settings::load_from(&p))
    }

    /// The settings saved at `path`: the defaults when there is no file; when the file is there but cannot be read
    /// or does not parse, the defaults with every run log kept ([`Settings::unreadable`]).
    fn load_from(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => Settings::parse(&text).unwrap_or_else(Settings::unreadable),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(_) => Settings::unreadable(),
        }
    }

    /// The defaults for a settings file that is there but cannot be read: a player who has played before, whose
    /// run logs must not be deleted because the file broke, so [`Settings::keep_all_logs`] is on (and saved so).
    fn unreadable() -> Settings {
        Settings { keep_all_logs: true, ..Settings::default() }
    }

    /// Settings from saved JSON. Every save stores the whole struct, so
    /// installs from before 0.7.1 hold its default of 400 playouts: that
    /// count is raised to the current default.
    fn parse(text: &str) -> Option<Settings> {
        let mut s: Settings = serde_json::from_str(text).ok()?;
        if s.rollouts == 400 {
            s.rollouts = Settings::default().rollouts;
        }
        Some(s)
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Settings::path().ok_or("APPDATA is not set")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The held instruments; unknown keys are dropped.
    pub fn held_tools(&self) -> Vec<Tool> {
        self.tools.iter().filter_map(|k| Tool::from_key(k)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn float_places_read_from_old_new_and_broken_files() {
        // An older file has no field: no window placed.
        let old: Settings = serde_json::from_str(r#"{"rollouts": 1600}"#).unwrap();
        assert!(old.float_pos.is_empty());
        // Saved and read back.
        let mut s = Settings::default();
        s.float_pos.insert("tools".to_string(), [-1200, 340]);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.float_pos.get("tools"), Some(&[-1200, 340]));
        // A broken entry is left out alone; the other entries and fields still read.
        let broken: Settings = serde_json::from_str(r#"{"float_pos": {"tools": [10, 20], "run_check": "x", "whats_new": [1.5, 2]}, "rollouts": 900}"#).unwrap();
        assert_eq!(broken.float_pos.len(), 1);
        assert_eq!(broken.float_pos.get("tools"), Some(&[10, 20]));
        assert_eq!(broken.rollouts, 900);
        // Not a map at all, or null: none, the file still read.
        for text in [r#"{"float_pos": 5, "rollouts": 900}"#, r#"{"float_pos": null, "rollouts": 900}"#] {
            let s: Settings = serde_json::from_str(text).unwrap();
            assert!(s.float_pos.is_empty());
            assert_eq!(s.rollouts, 900);
        }
    }

    #[test]
    fn old_hotkey_field() {
        let s: Settings = serde_json::from_str(r#"{"hotkey": "F7", "rollouts": 300}"#).unwrap();
        assert_eq!((s.read_hotkey.as_str(), s.auto_hotkey.as_str(), s.rollouts, s.auto_place, s.tools_days, s.theme, s.run_log), ("F9", "F8", 300, true, None, None, true));
        assert!(s.check_updates && s.offer_install);
        // Old settings files have no dismissed "Updated to" notice.
        assert_eq!(s.updated_seen, "");
        assert_eq!(s.opacity, 1.0);
        // Old settings files have no click-through: it starts off as before.
        assert!(!s.click_through);
        assert_eq!((s.share_logs, s.shared_from.as_str(), s.install_id.as_str()), (None, "", ""));
        // Old settings files have no Vorax fields: use the defaults.
        assert_eq!((s.vorax_enabled, s.vorax_rollouts, s.vorax_learn, s.vorax_strips), (true, 4096, true, false));
        // Old settings files have no "Keep all run logs": on, so nothing is deleted; a new player gets off.
        assert!(s.keep_all_logs);
        assert!(!Settings::default().keep_all_logs);
        let on: Settings = serde_json::from_str(r#"{"keep_all_logs": true}"#).unwrap();
        let off: Settings = serde_json::from_str(r#"{"keep_all_logs": false}"#).unwrap();
        assert!(on.keep_all_logs && !off.keep_all_logs);
        // Every save writes the field.
        assert_eq!(serde_json::to_value(Settings::default()).unwrap()["keep_all_logs"], serde_json::json!(false));
    }

    /// "Start as administrator": off by default and in an older file without it; on is written and read back.
    #[test]
    fn start_as_admin_saved_and_off_in_older_files() {
        let old: Settings = serde_json::from_str(r#"{"read_hotkey": "F1"}"#).unwrap();
        assert!(!old.start_as_admin, "an older file: off");
        assert!(!Settings::default().start_as_admin);
        let on: Settings = serde_json::from_str(r#"{"start_as_admin": true}"#).unwrap();
        assert!(on.start_as_admin);
        let back: Settings = serde_json::from_value(serde_json::to_value(&on).unwrap()).unwrap();
        assert!(back.start_as_admin, "written and read back");
    }

    /// The click-through hotkey is gone (1.0.2): an older file's `through_hotkey` is ignored, not an error, and its
    /// click-through stays as it was saved; a save no longer writes the field.
    #[test]
    fn old_through_hotkey_is_ignored() {
        let s: Settings = serde_json::from_str(r#"{"through_hotkey": "F7", "click_through": true, "read_hotkey": "F1"}"#).unwrap();
        assert!(s.click_through);
        assert_eq!(s.read_hotkey, "F1");
        assert!(serde_json::to_value(&s).unwrap().get("through_hotkey").is_none());
    }

    /// Loading "Keep all run logs": no file gives the defaults (off); a file without the field (an older version's)
    /// or a file that does not parse gives on; a file with the field gives its value.
    #[test]
    fn keep_all_logs_on_load() {
        let root = appdata("keep-all");
        let path = root.join("settings.json");
        assert!(!Settings::load_from(&path).keep_all_logs, "no file");
        std::fs::write(&path, r#"{"rollouts": 300}"#).unwrap();
        let old = Settings::load_from(&path);
        assert!(old.keep_all_logs && old.rollouts == 300, "an older version's file");
        std::fs::write(&path, "{ broken").unwrap();
        assert!(Settings::load_from(&path).keep_all_logs, "a file that does not parse");
        for keep in [true, false] {
            std::fs::write(&path, serde_json::to_string_pretty(&Settings { keep_all_logs: keep, ..Settings::default() }).unwrap()).unwrap();
            assert_eq!(Settings::load_from(&path).keep_all_logs, keep, "saved {keep}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stand-in %APPDATA% for each test.
    fn appdata(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ascendant-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn home_copied_from_old_folder() {
        let root = appdata("home-old");
        std::fs::create_dir_all(root.join("warden")).unwrap();
        std::fs::write(root.join("warden").join("home.txt"), "F:\\games\\home\r\n").unwrap();
        assert_eq!(super::home_in(&root), Some(std::path::PathBuf::from(r"F:\games\home")));
        // Copied to the new folder; the old one stays.
        assert_eq!(std::fs::read_to_string(root.join("Ascendant").join("home.txt")).unwrap(), "F:\\games\\home\r\n");
        assert!(root.join("warden").join("home.txt").exists());
        // The new one takes priority.
        std::fs::write(root.join("Ascendant").join("home.txt"), r"D:\other").unwrap();
        assert_eq!(super::home_in(&root), Some(std::path::PathBuf::from(r"D:\other")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_home_without_a_file() {
        let root = appdata("home-none");
        assert_eq!(super::home_in(&root), None);
        assert!(!root.join("Ascendant").exists());
        // The old home.txt is not an absolute path: not copied and no new folder is created, dir_in still uses warden.
        std::fs::create_dir_all(root.join("warden")).unwrap();
        std::fs::write(root.join("warden").join("home.txt"), "relative\\home").unwrap();
        assert_eq!(super::home_in(&root), None);
        assert!(!root.join("Ascendant").exists());
        assert_eq!(super::dir_in(&root), root.join("warden"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_folder_kept_until_a_new_one() {
        let root = appdata("dir");
        assert_eq!(super::dir_in(&root), root.join("Ascendant"));
        std::fs::create_dir_all(root.join("warden")).unwrap();
        assert_eq!(super::dir_in(&root), root.join("warden"));
        std::fs::create_dir_all(root.join("Ascendant")).unwrap();
        assert_eq!(super::dir_in(&root), root.join("Ascendant"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_default_rollouts_raised() {
        let rollouts = |text: &str| Settings::parse(text).map(|s| s.rollouts);
        assert_eq!(rollouts(r#"{"rollouts": 400}"#), Some(1600));
        assert_eq!(rollouts(r#"{"rollouts": 300}"#), Some(300));
        assert_eq!(rollouts("{}"), Some(1600));
        assert_eq!(rollouts("not json"), None);
    }
}
