use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub projects: Vec<ProjectEntry>,
}

fn global_config_dir() -> PathBuf {
    dirs_path().join("bork")
}

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

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn global_config_path() -> PathBuf {
    global_config_dir().join("projects.json")
}

pub fn load_global_config() -> GlobalConfig {
    let path = global_config_path();
    if !path.exists() {
        return GlobalConfig::default();
    }
    let Ok(contents) = fs::read_to_string(&path) else {
        return GlobalConfig::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn save_global_config(config: &GlobalConfig) -> anyhow::Result<()> {
    let dir = global_config_dir();
    fs::create_dir_all(&dir)?;

    let path = global_config_path();
    let json = serde_json::to_string_pretty(config)?;

    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, &path)?;

    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn register_project(name: &str, path: &Path) -> anyhow::Result<()> {
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
        });
    }

    save_global_config(&config)
}

pub fn register_if_absent(name: &str, path: &Path) -> anyhow::Result<()> {
    let canonical = normalize_path(path);
    let config = load_global_config();

    let already_registered = config
        .projects
        .iter()
        .any(|e| normalize_path(&e.path) == canonical);

    if !already_registered {
        return register_project(name, path);
    }
    Ok(())
}

pub fn unregister_project(path: &Path) -> anyhow::Result<bool> {
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

pub fn prune_stale_projects() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that modify XDG_CONFIG_HOME
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_config(name: &str, test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir =
            std::env::temp_dir().join(format!("bork-test-global-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let old = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };

        test();

        if let Some(v) = old {
            unsafe { std::env::set_var("XDG_CONFIG_HOME", v) };
        } else {
            unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn make_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bork-proj-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_empty_returns_default() {
        with_temp_config("empty", || {
            let config = load_global_config();
            assert!(config.projects.is_empty());
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
}
