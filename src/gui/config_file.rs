//! Edits this computer's herdr `config.toml` from the desktop app, keeping the user's
//! comments and layout. The server picks changes up through `server.reload_config`.

/// Sets `[table] key = value`, creating the table when needed.
pub(crate) fn set(table: &str, key: &str, value: toml_edit::Value) -> Result<(), String> {
    let path = crate::config::config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let updated =
        set_in(&text, table, key, value).map_err(|error| format!("{}: {error}", path.display()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, updated).map_err(|e| e.to_string())
}

fn set_in(text: &str, table: &str, key: &str, value: toml_edit::Value) -> Result<String, String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| format!("not valid TOML: {error}"))?;
    let table = document
        .entry(table)
        .or_insert(toml_edit::table())
        .as_table_mut()
        .ok_or_else(|| format!("[{table}] is not a table"))?;
    table[key] = toml_edit::Item::Value(value);
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_keeps_comments_and_other_keys() {
        let text =
            "# my config\n[theme]\n# pick one\nname = \"nord\"\n\n[ui]\nmouse_capture = false\n";
        let updated = set_in(text, "theme", "name", "dracula".into()).unwrap();
        assert!(updated.contains("# my config"));
        assert!(updated.contains("# pick one"));
        assert!(updated.contains("name = \"dracula\""));
        assert!(updated.contains("mouse_capture = false"));
        let added = set_in(&updated, "ui", "max_fps", 120i64.into()).unwrap();
        assert!(added.contains("max_fps = 120"));
        let created = set_in("", "theme", "name", "vesper".into()).unwrap();
        assert_eq!(created.trim(), "[theme]\nname = \"vesper\"");
    }
}
