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

/// Install bork hooks for both OpenCode and Claude Code.
pub fn install() -> anyhow::Result<()> {
    install_opencode_plugin()?;
    install_claude_hooks()?;
    println!("bork hooks installed successfully");
    Ok(())
}

/// Remove bork hooks from both OpenCode and Claude Code.
pub fn uninstall() -> anyhow::Result<()> {
    uninstall_opencode_plugin()?;
    uninstall_claude_hooks()?;
    println!("bork hooks uninstalled successfully");
    Ok(())
}

fn opencode_plugin_path() -> PathBuf {
    let config_dir = dirs_global_config().join("opencode").join("plugins");
    config_dir.join("bork-status.ts")
}

fn install_opencode_plugin() -> anyhow::Result<()> {
    let path = opencode_plugin_path();
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

fn install_claude_hooks() -> anyhow::Result<()> {
    let path = claude_settings_path();

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
            let existing_arr = existing_entries.as_array_mut().unwrap();

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

fn dirs_global_config() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config")
}
