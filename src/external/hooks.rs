use std::fs;
use std::path::PathBuf;

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

// Codex hooks support fewer lifecycle events than Claude Code. There is no
// PermissionRequest or Notification event, so WaitingPermission/WaitingInput
// statuses are not available for Codex sessions. SessionStart sets Idle on
// launch so the card shows a status immediately.
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

fn claude_hooks_already_installed(path: &PathBuf) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    let Some(hooks) = settings.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };
    let Ok(bork_hooks) = serde_json::from_str::<serde_json::Value>(CLAUDE_HOOKS) else {
        return false;
    };
    let Some(bork_hooks_obj) = bork_hooks.as_object() else {
        return false;
    };

    for (event_name, bork_entries) in bork_hooks_obj {
        let Some(existing) = hooks.get(event_name).and_then(|v| v.as_array()) else {
            return false;
        };
        let Some(expected) = bork_entries.as_array() else {
            return false;
        };
        // Check that every expected bork hook entry exists in the current hooks
        for entry in expected {
            if !existing.contains(entry) {
                return false;
            }
        }
    }
    true
}

fn install_claude_hooks() -> anyhow::Result<()> {
    let path = claude_settings_path();

    if path.exists() && claude_hooks_already_installed(&path) {
        println!("  Claude Code hooks already installed (skipped)");
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut settings: serde_json::Value = if path.exists() {
        let contents = fs::read_to_string(&path)?;
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hooks_obj = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json is not an object"))?;

    if !hooks_obj.contains_key("hooks") {
        hooks_obj.insert("hooks".to_string(), serde_json::json!({}));
    }

    let existing_hooks = hooks_obj
        .get_mut("hooks")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("hooks is not an object"))?;

    let bork_hooks: serde_json::Value = serde_json::from_str(CLAUDE_HOOKS)?;
    let bork_hooks_obj = bork_hooks.as_object().unwrap();

    for (event_name, bork_entries) in bork_hooks_obj {
        let bork_entries = bork_entries.as_array().unwrap();

        if let Some(existing_entries) = existing_hooks.get_mut(event_name) {
            let Some(existing_arr) = existing_entries.as_array_mut() else {
                continue;
            };

            // Remove any existing bork hooks (identified by BORK_STATUS_DIR in the command)
            existing_arr.retain(|entry| !is_bork_hook(entry));

            // Add the new bork hooks
            for entry in bork_entries {
                existing_arr.push(entry.clone());
            }
        } else {
            existing_hooks.insert(
                event_name.clone(),
                serde_json::Value::Array(bork_entries.clone()),
            );
        }
    }

    let json = serde_json::to_string_pretty(&settings)?;
    fs::write(&path, format!("{}\n", json))?;
    println!("  Claude Code hooks installed in {}", path.display());
    Ok(())
}

fn uninstall_claude_hooks() -> anyhow::Result<()> {
    let path = claude_settings_path();

    if !path.exists() {
        println!("  Claude Code settings not found (already removed)");
        return Ok(());
    }

    let contents = fs::read_to_string(&path)?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}));

    let Some(hooks_obj) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        println!("  No Claude Code hooks found (already removed)");
        return Ok(());
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
    fs::write(&path, format!("{}\n", json))?;
    println!("  Claude Code hooks removed from {}", path.display());
    Ok(())
}

fn codex_hooks_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".codex").join("hooks.json")
}

fn codex_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".codex").join("config.toml")
}

fn codex_hooks_already_installed(path: &PathBuf) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    let Some(hooks) = settings.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };
    let Ok(bork_hooks) = serde_json::from_str::<serde_json::Value>(CODEX_HOOKS) else {
        return false;
    };
    let Some(bork_hooks_obj) = bork_hooks.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };

    for (event_name, bork_entries) in bork_hooks_obj {
        let Some(existing) = hooks.get(event_name).and_then(|v| v.as_array()) else {
            return false;
        };
        let Some(expected) = bork_entries.as_array() else {
            return false;
        };
        for entry in expected {
            if !existing.contains(entry) {
                return false;
            }
        }
    }
    true
}

fn install_codex_hooks() -> anyhow::Result<()> {
    ensure_codex_hooks_feature_enabled()?;

    let path = codex_hooks_path();
    if path.exists() && codex_hooks_already_installed(&path) {
        println!("  Codex hooks already installed (skipped)");
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut settings: serde_json::Value = if path.exists() {
        let contents = fs::read_to_string(&path)?;
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hooks_obj = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks.json is not an object"))?;

    if !hooks_obj.contains_key("hooks") {
        hooks_obj.insert("hooks".to_string(), serde_json::json!({}));
    }

    let existing_hooks = hooks_obj
        .get_mut("hooks")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("hooks is not an object"))?;

    let bork_hooks: serde_json::Value = serde_json::from_str(CODEX_HOOKS)?;
    let bork_hooks_obj = bork_hooks
        .get("hooks")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("codex hooks payload is invalid"))?;

    for (event_name, bork_entries) in bork_hooks_obj {
        let Some(bork_entries) = bork_entries.as_array() else {
            continue;
        };

        if let Some(existing_entries) = existing_hooks.get_mut(event_name) {
            let Some(existing_arr) = existing_entries.as_array_mut() else {
                continue;
            };
            existing_arr.retain(|entry| !is_bork_hook(entry));
            for entry in bork_entries {
                existing_arr.push(entry.clone());
            }
        } else {
            existing_hooks.insert(
                event_name.clone(),
                serde_json::Value::Array(bork_entries.clone()),
            );
        }
    }

    let json = serde_json::to_string_pretty(&settings)?;
    fs::write(&path, format!("{}\n", json))?;
    println!("  Codex hooks installed in {}", path.display());
    Ok(())
}

fn uninstall_codex_hooks() -> anyhow::Result<()> {
    let path = codex_hooks_path();
    if !path.exists() {
        println!("  Codex hooks not found (already removed)");
        return Ok(());
    }

    let contents = fs::read_to_string(&path)?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}));

    let Some(hooks_obj) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        println!("  No Codex hooks found (already removed)");
        return Ok(());
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
    fs::write(&path, format!("{}\n", json))?;
    println!("  Codex hooks removed from {}", path.display());
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
    if let Some(hooks_arr) = entry.get("hooks").and_then(|v| v.as_array()) {
        for hook in hooks_arr {
            if let Some(cmd) = hook.get("command").and_then(|v| v.as_str()) {
                if cmd.contains("BORK_STATUS_DIR") {
                    return true;
                }
            }
        }
    }
    false
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

    // --- claude_hooks_already_installed ---
    // This function reads from a file, but we can test the JSON logic
    // by writing temp files.

    #[test]
    fn hooks_installed_detects_full_match() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-installed.json");
        let settings = json!({
            "hooks": serde_json::from_str::<serde_json::Value>(CLAUDE_HOOKS).unwrap()
        });
        std::fs::write(&tmp, serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(claude_hooks_already_installed(&tmp.to_path_buf()));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn hooks_installed_false_when_no_file() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-nonexistent.json");
        assert!(!claude_hooks_already_installed(&tmp.to_path_buf()));
    }

    #[test]
    fn hooks_installed_false_when_empty_hooks() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-empty.json");
        let settings = json!({"hooks": {}});
        std::fs::write(&tmp, serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(!claude_hooks_already_installed(&tmp.to_path_buf()));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn hooks_installed_false_when_no_hooks_key() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-nokey.json");
        let settings = json!({"other": "stuff"});
        std::fs::write(&tmp, serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(!claude_hooks_already_installed(&tmp.to_path_buf()));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn hooks_installed_false_when_invalid_json() {
        let tmp = std::env::temp_dir().join("bork-test-hooks-invalid.json");
        std::fs::write(&tmp, "not json").unwrap();
        assert!(!claude_hooks_already_installed(&tmp.to_path_buf()));
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
