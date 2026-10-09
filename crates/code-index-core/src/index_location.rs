//! One source of truth for the database directory of an indexed path.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::daemon_core::config::{self, PathEntry};

pub fn validate_entries(entries: &[PathEntry]) -> Result<()> {
    let mut dirs = std::collections::HashSet::new();
    for entry in entries {
        let Some(dir) = &entry.index_dir else { continue };
        if !dir.is_absolute() {
            bail!("index_dir должен быть абсолютным: {}", dir.display());
        }
        let key = normalized(dir);
        if !dirs.insert(key.clone()) {
            bail!("Один index_dir задан для нескольких путей: {}", dir.display());
        }
        for other in entries {
            let root = normalized(&other.path);
            if key == root || key.starts_with(&(root + "/")) {
                bail!("index_dir должен находиться вне индексируемых каталогов: {}", dir.display());
            }
        }
    }
    Ok(())
}

fn normalized(path: &Path) -> String {
    // Канонизация здесь — только для СРАВНЕНИЯ (приведение разных написаний
    // одного пути к одному ключу), в ФС по результату не ходим. Семантику не
    // меняем: verbatim-префикс снимается, дальше lowercase и прямые слеши.
    crate::paths::canonicalize(path)
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

pub fn directory_for_entry(entry: &PathEntry) -> Result<PathBuf> {
    // I/O: от корня строится внутренний каталог `.code-index` и проверяется
    // наличие старого каталога, поэтому путь обязан быть в обычной форме —
    // verbatim-путь ломает создание каталога и сателлитов SQLite на Windows.
    let root = crate::paths::canonicalize(&entry.path);
    match &entry.index_dir {
        Some(dir) => {
            if !dir.is_absolute() {
                bail!("index_dir должен быть абсолютным: {}", dir.display());
            }
            let root_key = normalized(&root);
            let dir_key = normalized(dir);
            if dir_key == root_key || dir_key.starts_with(&(root_key + "/")) {
                bail!("index_dir должен находиться вне индексируемого каталога: {}", dir.display());
            }
            if root.join(".code-index").exists() {
                bail!("Есть старый каталог индекса {}: сначала перенесите его в {}",
                    root.join(".code-index").display(), dir.display());
            }
            Ok(dir.clone())
        }
        None => Ok(root.join(".code-index")),
    }
}

pub fn directory_for_path(root: &Path, config_path: Option<&Path>) -> Result<PathBuf> {
    let cfg = match config_path {
        Some(path) => Some(config::load_from(path)?),
        None => {
            let path = match std::env::var("CODE_INDEX_HOME") {
                Ok(home) if !home.is_empty() => PathBuf::from(home).join("daemon.toml"),
                _ => return Ok(root.join(".code-index")),
            };
            if path.exists() { Some(config::load_from(&path)?) } else { None }
        }
    };
    if let Some(cfg) = cfg {
        let key = normalized(root);
        if let Some(entry) = cfg.paths.iter().find(|p| normalized(&p.path) == key) {
            return directory_for_entry(entry);
        }
    }
    Ok(root.join(".code-index"))
}

pub fn db_for_path(root: &Path, config_path: Option<&Path>) -> Result<PathBuf> {
    Ok(directory_for_path(root, config_path)?.join("index.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_directory_and_legacy_fallback() {
        let base = std::env::temp_dir().join(format!("code-index-location-{}", std::process::id()));
        let root = base.join("source");
        let target = base.join("indexes").join("one");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = base.join("daemon.toml");
        std::fs::write(&cfg, format!("[[paths]]\npath = '{}'\nindex_dir = '{}'\n",
            root.display().to_string().replace('\\', "/"),
            target.display().to_string().replace('\\', "/"))).unwrap();
        assert_eq!(db_for_path(&root, Some(&cfg)).unwrap(), target.join("index.db"));
        assert_eq!(db_for_path(&base.join("other"), Some(&cfg)).unwrap(),
                   base.join("other").join(".code-index").join("index.db"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rejects_relative_nested_and_shared_directories() {
        let base = std::env::temp_dir().join(format!("code-index-invalid-{}", std::process::id()));
        let root = base.join("source");
        let other = base.join("other");
        let outside = base.join("indexes");
        for invalid in [PathBuf::from("relative"), root.join("inside")] {
            let cfg = format!("[[paths]]\npath = '{}'\nindex_dir = '{}'\n",
                root.display().to_string().replace('\\', "/"),
                invalid.display().to_string().replace('\\', "/"));
            assert!(config::parse_str(&cfg).is_err());
        }
        let cfg = format!("[[paths]]\npath = '{}'\nindex_dir = '{}'\n\
                           [[paths]]\npath = '{}'\nindex_dir = '{}'\n",
            root.display().to_string().replace('\\', "/"),
            outside.display().to_string().replace('\\', "/"),
            other.display().to_string().replace('\\', "/"),
            outside.display().to_string().replace('\\', "/"));
        assert!(config::parse_str(&cfg).is_err());
    }

    /// Приёмка 7а: построенный каталог индекса не несёт verbatim-префикса —
    /// ни во внутренней ветке, ни в ветке `index_dir`. На Windows именно
    /// `\\?\`-форма ломала создание каталога и сателлитов SQLite.
    #[test]
    fn built_directory_has_no_verbatim_prefix() {
        let base = std::env::temp_dir().join(format!("code-index-verbatim-{}", std::process::id()));
        let root = base.join("source");
        std::fs::create_dir_all(&root).unwrap();

        let entry_no_dir = PathEntry {
            path: root.clone(),
            index_dir: None,
            debounce_ms: None,
            batch_ms: None,
            alias: None,
            language: None,
            max_code_file_size_bytes: None,
            bulk_batch_threshold: None,
        };
        let internal = directory_for_entry(&entry_no_dir).unwrap();
        assert!(
            !crate::paths::is_verbatim(&internal),
            "внутренний каталог индекса в verbatim-форме: {}",
            internal.display()
        );
        assert!(internal.ends_with(".code-index"));

        let target = base.join("indexes").join("one");
        let entry_with_dir = PathEntry {
            index_dir: Some(target.clone()),
            ..entry_no_dir
        };
        let external = directory_for_entry(&entry_with_dir).unwrap();
        assert_eq!(external, target);
        assert!(
            !crate::paths::is_verbatim(&external),
            "каталог из index_dir в verbatim-форме: {}",
            external.display()
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn refuses_old_directory_until_migration() {
        let base = std::env::temp_dir().join(format!("code-index-old-{}", std::process::id()));
        let root = base.join("source");
        let old = root.join(".code-index");
        std::fs::create_dir_all(&old).unwrap();
        let cfg = base.join("daemon.toml");
        std::fs::write(&cfg, format!("[[paths]]\npath = '{}'\nindex_dir = '{}'\n",
            root.display().to_string().replace('\\', "/"),
            base.join("new").display().to_string().replace('\\', "/"))).unwrap();
        assert!(db_for_path(&root, Some(&cfg)).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
