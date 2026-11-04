use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::ProjectId;
use crate::config::{AppConfig, AppState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Follows `project_name` in the project's config.toml.
    pub name: String,
    pub path: PathBuf,
    /// Name set from the sidebar. Shown instead of `name` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub projects: Vec<ProjectEntry>,
}

pub fn global_config_dir() -> PathBuf {
    dirs_path().join("bork")
}

#[cfg(not(test))]
fn dirs_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    if let Some(home) = home_dir() {
        return home.join(".config");
    }
    // Fallback: $HOME and $XDG_CONFIG_HOME both unset (broken environment).
    // Creates config relative to cwd, which is non-ideal but avoids a hard error.
    PathBuf::from(".config")
}

// Tests never read XDG_CONFIG_HOME or HOME: a developer's exported value
// would point tests at their real project registry. Tests that need a
// specific directory set it through with_test_config_home.
#[cfg(test)]
fn dirs_path() -> PathBuf {
    TEST_CONFIG_HOME
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("bork-test-config-{}", std::process::id()))
        })
}

#[cfg(test)]
static TEST_CONFIG_HOME: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Runs `test` with the global config dir rooted at `dir`. Holds
/// XDG_ENV_LOCK so tests that share the override never overlap.
#[cfg(test)]
pub(crate) fn with_test_config_home(dir: &Path, test: impl FnOnce()) {
    let _guard = crate::XDG_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    *TEST_CONFIG_HOME.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
    test();
    *TEST_CONFIG_HOME.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(not(test))]
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn global_config_path() -> PathBuf {
    global_config_dir().join("projects.json")
}

pub fn load_global_config() -> GlobalConfig {
    read_global_config().unwrap_or_default()
}

/// `None` when the registry is missing or unreadable, so callers can tell
/// "no projects" apart from "couldn't read the file".
fn read_global_config() -> Option<GlobalConfig> {
    let contents = fs::read_to_string(global_config_path()).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn save_global_config(config: &GlobalConfig) -> anyhow::Result<()> {
    let dir = global_config_dir();
    fs::create_dir_all(&dir)?;

    let path = global_config_path();
    let json = serde_json::to_string_pretty(config)?;

    crate::config::atomic_write(&path, json.as_bytes())?;

    Ok(())
}

fn lock_config_file() -> Option<File> {
    let dir = global_config_dir();
    let _ = fs::create_dir_all(&dir);
    let lock_path = dir.join("projects.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if ret != 0 {
        return None;
    }
    Some(file)
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn register_project(name: &str, path: &Path) -> anyhow::Result<()> {
    let _lock = lock_config_file();
    let canonical = normalize_path(path);
    let mut config = load_global_config();

    if let Some(entry) = config
        .projects
        .iter_mut()
        .find(|e| normalize_path(&e.path) == canonical)
    {
        entry.name = name.to_string();
        entry.path = canonical;
    } else {
        config.projects.push(ProjectEntry {
            name: name.to_string(),
            path: canonical,
            label: None,
        });
    }

    save_global_config(&config)
}

pub fn register_if_absent(name: &str, path: &Path) -> anyhow::Result<()> {
    let _lock = lock_config_file();
    let canonical = normalize_path(path);
    let config = load_global_config();

    let already_registered = config
        .projects
        .iter()
        .any(|e| normalize_path(&e.path) == canonical);

    if !already_registered {
        let mut config = config;
        config.projects.push(ProjectEntry {
            name: name.to_string(),
            path: canonical,
            label: None,
        });
        save_global_config(&config)?;
    }
    Ok(())
}

pub fn unregister_project(path: &Path) -> anyhow::Result<bool> {
    let _lock = lock_config_file();
    let canonical = normalize_path(path);
    let mut config = load_global_config();
    let before = config.projects.len();

    config
        .projects
        .retain(|e| normalize_path(&e.path) != canonical);

    let removed = config.projects.len() < before;
    if removed {
        save_global_config(&config)?;
    }
    Ok(removed)
}

/// Sets or clears the sidebar label for `path` and returns the saved
/// registry. A label can't match another project's name or label, since
/// `--project` resolves by both.
pub fn set_project_label(path: &Path, label: Option<&str>) -> anyhow::Result<Vec<ProjectEntry>> {
    let _lock = lock_config_file();
    let canonical = normalize_path(path);
    let mut config = load_global_config();

    if let Some(label) = label {
        let taken = config.projects.iter().any(|entry| {
            normalize_path(&entry.path) != canonical
                && (entry.name == label || entry.label.as_deref() == Some(label))
        });
        if taken {
            anyhow::bail!("A project named '{}' already exists", label);
        }
    }
    let entry = config
        .projects
        .iter_mut()
        .find(|e| normalize_path(&e.path) == canonical)
        .ok_or_else(|| anyhow::anyhow!("Project is not registered"))?;
    entry.label = label.map(str::to_string);

    save_global_config(&config)?;
    Ok(config.projects)
}

/// Sorts the registry to follow `order` and returns the saved registry.
/// Entries missing from `order` keep their relative order at the end.
pub fn set_project_order(order: &[PathBuf]) -> anyhow::Result<Vec<ProjectEntry>> {
    let _lock = lock_config_file();
    let mut config = load_global_config();

    config.projects.sort_by_key(|entry| {
        let canonical = normalize_path(&entry.path);
        order
            .iter()
            .position(|path| *path == canonical)
            .unwrap_or(usize::MAX)
    });

    save_global_config(&config)?;
    Ok(config.projects)
}

pub fn prune_stale_projects() {
    let _lock = lock_config_file();
    let mut config = load_global_config();
    let before = config.projects.len();
    config
        .projects
        .retain(|e| e.path.join(".bork").join("config.toml").exists());
    if config.projects.len() < before {
        let _ = save_global_config(&config);
    }
}

pub fn list_projects() -> Vec<ProjectEntry> {
    load_global_config().projects
}

#[derive(Default)]
pub struct ReloadResult {
    pub new_projects: Vec<(AppConfig, AppState)>,
    /// Loaded projects that are no longer in the registry (deleted or unregistered).
    pub removed_projects: Vec<ProjectId>,
}

impl ReloadResult {
    pub fn is_empty(&self) -> bool {
        self.new_projects.is_empty() && self.removed_projects.is_empty()
    }
}

pub fn discover_new_projects(known_roots: HashSet<ProjectId>) -> ReloadResult {
    prune_stale_projects();

    // A bad read must not look like every project was unregistered.
    let Some(config) = read_global_config() else {
        return ReloadResult::default();
    };

    let mut new_projects = Vec::new();
    let mut registered_roots = HashSet::new();
    for entry in &config.projects {
        if !entry.path.join(".bork").join("config.toml").exists() {
            continue;
        }
        let canonical = normalize_path(&entry.path);
        registered_roots.insert(canonical.clone());
        if known_roots.contains(&canonical) {
            continue;
        }
        let proj_config = crate::config::load_config_from(&entry.path);
        let proj_state = crate::config::load_state(&entry.path);
        new_projects.push((proj_config, proj_state));
    }

    let removed_projects = known_roots
        .into_iter()
        .filter(|root| !registered_roots.contains(root))
        .collect();

    ReloadResult {
        new_projects,
        removed_projects,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn with_temp_config(name: &str, test: impl FnOnce()) {
        let dir =
            std::env::temp_dir().join(format!("bork-test-global-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        with_test_config_home(&dir, test);

        let _ = fs::remove_dir_all(&dir);
    }

    fn make_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bork-proj-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // A registered project carries .bork/config.toml in reality, and
        // prune_stale_projects drops entries without one. A sidebar-reload
        // test's background discover thread prunes concurrently, so a bare
        // directory here flakes any test that registered it.
        fs::create_dir_all(dir.join(".bork")).unwrap();
        fs::write(dir.join(".bork/config.toml"), "project_name = \"t\"\n").unwrap();
        dir
    }

    #[test]
    fn inherited_xdg_config_home_never_reaches_tests() {
        let _guard = crate::XDG_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let inherited =
            std::env::temp_dir().join(format!("bork-test-inherited-xdg-{}", std::process::id()));
        let old = std::env::var("XDG_CONFIG_HOME").ok();

        unsafe { std::env::set_var("XDG_CONFIG_HOME", &inherited) };
        let with_xdg = global_config_dir();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let without_xdg = global_config_dir();

        if let Some(v) = old {
            unsafe { std::env::set_var("XDG_CONFIG_HOME", v) };
        }

        assert!(!with_xdg.starts_with(&inherited), "{with_xdg:?}");
        assert_eq!(with_xdg, without_xdg);
    }

    #[test]
    fn load_empty_returns_default() {
        with_temp_config("empty", || {
            let config = load_global_config();
            assert!(config.projects.is_empty());
        });
    }

    #[test]
    fn discover_unreadable_registry_removes_nothing() {
        with_temp_config("unreadable", || {
            fs::create_dir_all(global_config_dir()).unwrap();
            fs::write(global_config_path(), "{ not json").unwrap();

            let known = HashSet::from([PathBuf::from("/some/project")]);
            let result = discover_new_projects(known);
            assert!(result.removed_projects.is_empty());
        });
    }

    #[test]
    fn discover_reports_unregistered_known_project() {
        with_temp_config("discover-removed", || {
            let dir = make_temp_dir("discover-removed");
            fs::create_dir_all(dir.join(".bork")).unwrap();
            fs::write(dir.join(".bork").join("config.toml"), "").unwrap();
            register_project("kept", &dir).unwrap();

            let kept = fs::canonicalize(&dir).unwrap();
            let gone = PathBuf::from("/deleted/project");
            let result = discover_new_projects(HashSet::from([kept, gone.clone()]));
            assert_eq!(result.removed_projects, vec![gone]);
            assert!(result.new_projects.is_empty());
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn register_and_list() {
        with_temp_config("register", || {
            let dir = make_temp_dir("register");

            register_project("test-proj", &dir).unwrap();
            let projects = list_projects();
            assert_eq!(projects.len(), 1);
            assert_eq!(projects[0].name, "test-proj");

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn register_idempotent() {
        with_temp_config("idempotent", || {
            let dir = make_temp_dir("idempotent");

            register_project("proj-v1", &dir).unwrap();
            register_project("proj-v2", &dir).unwrap();
            let projects = list_projects();
            assert_eq!(projects.len(), 1);
            assert_eq!(projects[0].name, "proj-v2");

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn label_sets_clears_and_rejects_duplicates() {
        with_temp_config("label", || {
            let dir1 = make_temp_dir("label1");
            let dir2 = make_temp_dir("label2");
            register_project("alpha", &dir1).unwrap();
            register_project("beta", &dir2).unwrap();

            let saved = set_project_label(&dir1, Some("Alpha (main)")).unwrap();
            assert_eq!(saved[0].label.as_deref(), Some("Alpha (main)"));
            assert_eq!(list_projects()[0].label.as_deref(), Some("Alpha (main)"));
            // Its own name and label are not duplicates.
            set_project_label(&dir1, Some("Alpha (main)")).unwrap();
            set_project_label(&dir1, Some("alpha")).unwrap();

            assert!(set_project_label(&dir2, Some("alpha")).is_err());
            set_project_label(&dir1, Some("Alpha (main)")).unwrap();
            assert!(set_project_label(&dir2, Some("Alpha (main)")).is_err());
            assert_eq!(list_projects()[1].label, None);
            assert!(set_project_label(Path::new("/not/registered"), Some("x")).is_err());

            set_project_label(&dir1, None).unwrap();
            assert_eq!(list_projects()[0].label, None);

            let _ = fs::remove_dir_all(&dir1);
            let _ = fs::remove_dir_all(&dir2);
        });
    }

    #[test]
    fn register_keeps_label_and_refreshes_name() {
        with_temp_config("register-label", || {
            let dir = make_temp_dir("register-label");
            register_project("old", &dir).unwrap();
            set_project_label(&dir, Some("Custom")).unwrap();

            register_project("new", &dir).unwrap();

            let entry = &list_projects()[0];
            assert_eq!(entry.name, "new");
            assert_eq!(entry.label.as_deref(), Some("Custom"));
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn set_order_sorts_known_and_keeps_unknown_last() {
        with_temp_config("order", || {
            let dirs: Vec<PathBuf> = ["order-a", "order-b", "order-c"]
                .iter()
                .map(|name| make_temp_dir(name))
                .collect();
            for (dir, name) in dirs.iter().zip(["a", "b", "c"]) {
                register_project(name, dir).unwrap();
            }

            let order = vec![normalize_path(&dirs[1]), normalize_path(&dirs[0])];
            let saved = set_project_order(&order).unwrap();
            let names: Vec<&str> = saved.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["b", "a", "c"]);
            assert_eq!(list_projects()[0].name, "b");

            for dir in &dirs {
                let _ = fs::remove_dir_all(dir);
            }
        });
    }

    #[test]
    fn unregister_removes_entry() {
        with_temp_config("unregister", || {
            let dir = make_temp_dir("unregister");

            register_project("doomed", &dir).unwrap();
            assert_eq!(list_projects().len(), 1);

            let removed = unregister_project(&dir).unwrap();
            assert!(removed);
            assert!(list_projects().is_empty());

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn unregister_missing_returns_false() {
        with_temp_config("unreg-missing", || {
            let removed =
                unregister_project(Path::new("/nonexistent/path/that/wont/exist")).unwrap();
            assert!(!removed);
        });
    }

    #[test]
    fn roundtrip_preserves_data() {
        with_temp_config("roundtrip", || {
            let dir1 = make_temp_dir("rt1");
            let dir2 = make_temp_dir("rt2");

            register_project("alpha", &dir1).unwrap();
            register_project("beta", &dir2).unwrap();

            let projects = list_projects();
            assert_eq!(projects.len(), 2);
            assert_eq!(projects[0].name, "alpha");
            assert_eq!(projects[1].name, "beta");

            let _ = fs::remove_dir_all(&dir1);
            let _ = fs::remove_dir_all(&dir2);
        });
    }

    #[test]
    fn register_if_absent_does_not_overwrite_name() {
        with_temp_config("if-absent", || {
            let dir = make_temp_dir("if-absent-proj");

            register_project("original-name", &dir).unwrap();
            register_if_absent("different-name", &dir).unwrap();

            let projects = list_projects();
            assert_eq!(projects.len(), 1);
            assert_eq!(
                projects[0].name, "original-name",
                "register_if_absent should not overwrite existing name"
            );

            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn prune_removes_entries_without_config_toml() {
        with_temp_config("prune-config", || {
            let dir = make_temp_dir("prune-proj");
            let bork_dir = dir.join(".bork");
            let _ = fs::create_dir_all(&bork_dir);
            fs::write(bork_dir.join("config.toml"), "project_name = \"test\"\n").unwrap();

            register_project("test", &dir).unwrap();
            assert_eq!(list_projects().len(), 1);

            // Remove config.toml but keep .bork/
            let _ = fs::remove_file(bork_dir.join("config.toml"));
            prune_stale_projects();
            assert_eq!(
                list_projects().len(),
                0,
                "prune should remove entries without config.toml"
            );

            let _ = fs::remove_dir_all(&dir);
        });
    }
}
