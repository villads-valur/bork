use std::fs;
use std::path::{Path, PathBuf};

const OPENCODE_PLUGIN: &str = include_str!("../../plugins/bork-status.ts");

const CLAUDE_HOOKS: &str = r#"{
  "UserPromptSubmit": [
    {
      "hooks": [
        {
          "type": "command",
          "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"Busy\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
        }
      ]
    }
  ],
  "PreToolUse": [
    {
      "matcher": "*",
      "hooks": [
        {
          "type": "command",
          "command": "TOOL=$(cat | grep -o '\"tool_name\":\"[^\"]*\"' | head -1 | cut -d'\"' -f4); [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"Busy\",\"activity\":\"%s\",\"updated_at\":%s}' \"$TOOL\" \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
        }
      ]
    }
  ],
  "PostToolUse": [
    {
      "matcher": "AskUserQuestion",
      "hooks": [
        {
          "type": "command",
          "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"WaitingInput\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
        }
      ]
    },
    {
      "matcher": "ExitPlanMode",
      "hooks": [
        {
          "type": "command",
          "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"WaitingApproval\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
        }
      ]
    }
  ],
  "Stop": [
    {
      "hooks": [
        {
          "type": "command",
          "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"Idle\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
        }
      ]
    }
  ],
  "Notification": [
    {
      "hooks": [
        {
          "type": "command",
          "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"WaitingInput\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
        }
      ]
    }
  ],
  "PermissionRequest": [
    {
      "hooks": [
        {
          "type": "command",
          "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"WaitingPermission\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
        }
      ]
    }
  ]
}"#;

// Codex hooks have fewer lifecycle events than Claude (no PermissionRequest or
// Notification), so WaitingPermission/WaitingInput statuses are unavailable.
const CODEX_HOOKS: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"Idle\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"Busy\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"Busy\",\"activity\":\"Bash\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "cat > /dev/null; [ -n \"$BORK_STATUS_DIR\" ] && [ -n \"$BORK_SESSION\" ] && printf '{\"status\":\"Idle\",\"updated_at\":%s}' \"$(date +%s)000\" > \"$BORK_STATUS_DIR/$BORK_SESSION.json\""
          }
        ]
      }
    ]
  }
}"#;

/// Install bork hooks for OpenCode, Claude Code, and Codex.
pub fn install() -> anyhow::Result<()> {
    install_opencode_plugin()?;
    install_claude_hooks()?;
    install_codex_hooks()?;
    println!("bork hooks installed successfully");
    Ok(())
}

/// Remove bork hooks from OpenCode, Claude Code, and Codex.
pub fn uninstall() -> anyhow::Result<()> {
    uninstall_opencode_plugin()?;
    uninstall_claude_hooks()?;
    uninstall_codex_hooks()?;
    println!("bork hooks uninstalled successfully");
    Ok(())
}

fn opencode_plugin_path() -> PathBuf {
    let config_dir = dirs_global_config().join("opencode").join("plugins");
    config_dir.join("bork-status.ts")
}

fn install_opencode_plugin() -> anyhow::Result<()> {
    let path = opencode_plugin_path();

    if path.exists() {
        if let Ok(existing) = fs::read_to_string(&path) {
            if existing == OPENCODE_PLUGIN {
                println!("  OpenCode plugin already installed (skipped)");
                return Ok(());
            }
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, OPENCODE_PLUGIN)?;
    println!("  OpenCode plugin installed at {}", path.display());
    Ok(())
}

fn uninstall_opencode_plugin() -> anyhow::Result<()> {
    let path = opencode_plugin_path();
    if path.exists() {
        fs::remove_file(&path)?;
        println!("  OpenCode plugin removed from {}", path.display());
    } else {
        println!("  OpenCode plugin not found (already removed)");
    }
    Ok(())
}

fn claude_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".claude").join("settings.json")
}

fn codex_hooks_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".codex").join("hooks.json")
}

fn codex_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".codex").join("config.toml")
}

// --- Shared hooks helpers ---

/// Check whether all expected hook entries are already present in `file[hooks_key]`.
fn json_hooks_already_installed(
    path: &PathBuf,
    expected: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    let Some(hooks) = settings.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };

    expected.iter().all(|(event_name, bork_entries)| {
        let Some(existing) = hooks.get(event_name).and_then(|v| v.as_array()) else {
            return false;
        };
        let Some(expected_arr) = bork_entries.as_array() else {
            return false;
        };
        expected_arr.iter().all(|entry| existing.contains(entry))
    })
}

/// Merge bork hook entries into `file["hooks"]`, replacing any existing bork hooks.
fn merge_hooks_into_file(
    path: &Path,
    expected: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut settings: serde_json::Value = if path.exists() {
        let contents = fs::read_to_string(path)?;
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let root = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not an object", path.display()))?;

    if !root.contains_key("hooks") {
        root.insert("hooks".to_string(), serde_json::json!({}));
    }

    let existing_hooks: &mut serde_json::Map<String, serde_json::Value> = root
        .get_mut("hooks")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("hooks is not an object"))?;

    for (event_name, bork_entries) in expected {
        let Some(bork_arr) = bork_entries.as_array() else {
            continue;
        };

        if let Some(existing_entries) = existing_hooks.get_mut(event_name) {
            let Some(existing_arr) = existing_entries.as_array_mut() else {
                continue;
            };
            existing_arr.retain(|entry| !is_bork_hook(entry));
            for entry in bork_arr {
                existing_arr.push(entry.clone());
            }
        } else {
            existing_hooks.insert(
                event_name.clone(),
                serde_json::Value::Array(bork_arr.clone()),
            );
        }
    }

    let json = serde_json::to_string_pretty(&settings)?;
    fs::write(path, format!("{}\n", json))?;
    Ok(())
}

/// Remove all bork hook entries from `file["hooks"]`, cleaning up empty events.
fn remove_hooks_from_file(path: &Path) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let contents = fs::read_to_string(path)?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}));

    let Some(hooks_obj) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return Ok(false);
    };

    let mut empty_events = Vec::new();
    for (event_name, entries) in hooks_obj.iter_mut() {
        if let Some(arr) = entries.as_array_mut() {
            arr.retain(|entry| !is_bork_hook(entry));
            if arr.is_empty() {
                empty_events.push(event_name.clone());
            }
        }
    }
    for event_name in empty_events {
        hooks_obj.remove(&event_name);
    }

    let json = serde_json::to_string_pretty(&settings)?;
    fs::write(path, format!("{}\n", json))?;
    Ok(true)
}

/// Parse the hooks map from a hook constant, optionally nested under a key.
fn parse_hook_entries(
    json_str: &str,
    nested_key: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return serde_json::Map::new();
    };
    let target = match nested_key {
        Some(key) => value.get(key).cloned().unwrap_or(serde_json::json!({})),
        None => value,
    };
    target
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new)
}

// --- Claude hooks ---

fn install_claude_hooks() -> anyhow::Result<()> {
    let path = claude_settings_path();
    let expected = parse_hook_entries(CLAUDE_HOOKS, None);

    if path.exists() && json_hooks_already_installed(&path, &expected) {
        println!("  Claude Code hooks already installed (skipped)");
        return Ok(());
    }

    merge_hooks_into_file(&path, &expected)?;
    println!("  Claude Code hooks installed in {}", path.display());
    Ok(())
}

fn uninstall_claude_hooks() -> anyhow::Result<()> {
    let path = claude_settings_path();
    if remove_hooks_from_file(&path)? {
        println!("  Claude Code hooks removed from {}", path.display());
    } else {
        println!("  Claude Code settings not found (already removed)");
    }
    Ok(())
}

// --- Codex hooks ---

fn install_codex_hooks() -> anyhow::Result<()> {
    ensure_codex_hooks_feature_enabled()?;

    let path = codex_hooks_path();
    let expected = parse_hook_entries(CODEX_HOOKS, Some("hooks"));

    if path.exists() && json_hooks_already_installed(&path, &expected) {
        println!("  Codex hooks already installed (skipped)");
        return Ok(());
    }

    merge_hooks_into_file(&path, &expected)?;
    println!("  Codex hooks installed in {}", path.display());
    Ok(())
}

fn uninstall_codex_hooks() -> anyhow::Result<()> {
    let path = codex_hooks_path();
    if remove_hooks_from_file(&path)? {
        println!("  Codex hooks removed from {}", path.display());
    } else {
        println!("  Codex hooks not found (already removed)");
    }
    Ok(())
}

fn ensure_codex_hooks_feature_enabled() -> anyhow::Result<()> {
    let path = codex_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut lines: Vec<String> = if path.exists() {
        fs::read_to_string(&path)?
            .lines()
            .map(ToString::to_string)
            .collect()
    } else {
        Vec::new()
    };

    let mut features_start = None;
    let mut features_end = lines.len();

    for (index, line) in lines.iter().enumerate() {
        if line.trim() == "[features]" {
            features_start = Some(index);
            break;
        }
    }

    if let Some(start) = features_start {
        for (index, line) in lines.iter().enumerate().skip(start + 1) {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                features_end = index;
                break;
            }
        }

        let mut updated = false;
        for line in lines.iter_mut().take(features_end).skip(start + 1) {
            if line.trim_start().starts_with("codex_hooks") {
                *line = "codex_hooks = true".to_string();
                updated = true;
                break;
            }
        }

        if !updated {
            lines.insert(features_end, "codex_hooks = true".to_string());
        }
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("[features]".to_string());
        lines.push("codex_hooks = true".to_string());
    }

    let mut output = lines.join("\n");
    output.push('\n');
    fs::write(&path, output)?;
    println!("  Codex hooks feature enabled in {}", path.display());
    Ok(())
}

fn is_bork_hook(entry: &serde_json::Value) -> bool {
    entry
        .get("hooks")
        .and_then(|v| v.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|v| v.as_str())
                    .is_some_and(|cmd| cmd.contains("BORK_STATUS_DIR"))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- is_bork_hook ---

    #[test]
    fn is_bork_hook_with_bork_command() {
        let entry = json!({
            "hooks": [{
                "type": "command",
                "command": "echo BORK_STATUS_DIR test"
            }]
        });
        assert!(is_bork_hook(&entry));
    }

    #[test]
    fn is_bork_hook_without_bork_command() {
        let entry = json!({
            "hooks": [{
                "type": "command",
                "command": "echo hello"
            }]
        });
        assert!(!is_bork_hook(&entry));
    }

    #[test]
    fn is_bork_hook_no_hooks_key() {
        let entry = json!({"type": "command"});
        assert!(!is_bork_hook(&entry));
    }

    #[test]
    fn is_bork_hook_empty_hooks_array() {
        let entry = json!({"hooks": []});
        assert!(!is_bork_hook(&entry));
    }

    #[test]
    fn is_bork_hook_hooks_not_array() {
        let entry = json!({"hooks": "not an array"});
        assert!(!is_bork_hook(&entry));
    }

    #[test]
    fn is_bork_hook_command_not_string() {
        let entry = json!({"hooks": [{"command": 42}]});
        assert!(!is_bork_hook(&entry));
    }

    // --- json_hooks_already_installed ---

    fn claude_hook_entries() -> serde_json::Map<String, serde_json::Value> {
        parse_hook_entries(CLAUDE_HOOKS, None)
    }

    #[test]
    fn hooks_installed_detects_full_match() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-installed.json");
        let settings = json!({
            "hooks": serde_json::from_str::<serde_json::Value>(CLAUDE_HOOKS).unwrap()
        });
        std::fs::write(&tmp, serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(json_hooks_already_installed(&tmp, &claude_hook_entries()));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn hooks_installed_false_when_no_file() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-nonexistent.json");
        assert!(!json_hooks_already_installed(&tmp, &claude_hook_entries()));
    }

    #[test]
    fn hooks_installed_false_when_empty_hooks() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-empty.json");
        let settings = json!({"hooks": {}});
        std::fs::write(&tmp, serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(!json_hooks_already_installed(&tmp, &claude_hook_entries()));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn hooks_installed_false_when_no_hooks_key() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-nokey.json");
        let settings = json!({"other": "stuff"});
        std::fs::write(&tmp, serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(!json_hooks_already_installed(&tmp, &claude_hook_entries()));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn hooks_installed_false_when_invalid_json() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-invalid.json");
        std::fs::write(&tmp, "not json").unwrap();
        assert!(!json_hooks_already_installed(&tmp, &claude_hook_entries()));
        let _ = std::fs::remove_file(&tmp);
    }
}

fn dirs_global_config() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config")
}
