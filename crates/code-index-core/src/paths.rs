//! Работа с путями: единая точка канонизации без Windows-verbatim-префикса.
//!
//! На Windows `std::fs::canonicalize()` возвращает путь в verbatim-форме с
//! префиксом `\\?\`. Чтение по такому пути работает, а вот создание каталогов
//! и файлов — нет: `CreateDirectoryW` по `\\?\…` отказывает (на узле с
//! фильтр-стеком KES/песочницы — `os error 5`, прямой вызов — `INVALID_NAME`),
//! и вместе с каталогом индекса падают сателлиты SQLite (`-wal`/`-shm`), что
//! даёт `readonly` (8), `cantopen` (14) и зависшее копирование базы в память.
//! Отсюда — требование: **ни один путь, уходящий в ФС или SQLite, не несёт
//! verbatim-префикса**. Канонизация нужна только для сравнения и отсечения
//! симлинков, а для файловых операций берётся обычная форма.
//!
//! Хелпер один на весь workspace: канонический путь — это ещё и ключ состояния
//! демона (`DaemonState.paths`, `workers`, `worker_entries`, `respawn_tracker`),
//! и воркер шлёт статусы по своему разрешённому пути. Упрощённый путь в одном
//! месте и verbatim в другом разнесли бы статус под другой ключ. Поэтому все
//! ключеобразующие точки обязаны давать одну форму — форму этих функций.

use std::path::{Path, PathBuf};

/// Снять Windows-verbatim-префикс, не обращаясь к ФС.
///
/// * `\\?\C:\Repo` → `C:\Repo`;
/// * `\\?\UNC\srv\share\x` → `\\srv\share\x` (сетевой путь остаётся сетевым);
/// * путь без префикса возвращается как есть.
///
/// Снимаем только две формы, которые реально даёт `std::fs::canonicalize` для
/// пользовательских путей, — диск и UNC. Прочие verbatim-namespace
/// (`\\?\Volume{…}`, `\\?\GLOBALROOT\…`) не трогаем: их «обычной формы» у нас
/// нет, а наивное срезание префикса превратило бы рабочий путь в мусор.
///
/// Не-UTF-8 путь тоже не трогаем: безопаснее оставить как есть, чем резать
/// префикс вслепую по байтам над `OsStr`.
pub fn simplify(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        // Обычная форма — только `X:\…`, где `X` — буква диска.
        let mut chars = rest.chars();
        let looks_like_disk = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
            && chars.next() == Some(':');
        if looks_like_disk {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// Канонизировать путь и привести к обычной форме (без verbatim-префикса).
///
/// Если канонизация не удалась (путь не существует, нет доступа) — возвращается
/// упрощённый исходный путь: так же, как это делали прежние вызовы
/// `std::fs::canonicalize().unwrap_or_else(|_| исходный)`. Для сравнения путей
/// этого достаточно, а для I/O лучше работать с исходной строкой, чем ни с чем.
pub fn canonicalize(path: &Path) -> PathBuf {
    // Единственный разрешённый вызов `std::fs::canonicalize` в рабочем коде
    // (см. `clippy.toml`, `disallowed-methods`). Любая новая точка должна идти
    // через этот хелпер — иначе verbatim-путь просочится в ФС или SQLite.
    #[allow(clippy::disallowed_methods)]
    let resolved = std::fs::canonicalize(path);
    resolved.map(|p| simplify(&p)).unwrap_or_else(|_| simplify(path))
}

/// Путь записан в verbatim-форме (`\\?\`)?
///
/// Для диагностики: отказ создания каталога/открытия БД по такому пути почти
/// всегда означает фильтр-стек (KES, песочница), а не права или диск. После
/// требования «путь в ФС без verbatim» таких путей быть не должно — проверка
/// страховочная.
pub fn is_verbatim(path: &Path) -> bool {
    path.to_str()
        .map(|s| s.starts_with(r"\\?\") || s.starts_with(r"\\.\"))
        .unwrap_or(false)
}

/// Хвост подсказки к ошибке для verbatim-пути. Пустая строка, если путь
/// обычный — тогда подсказка была бы шумом.
pub fn verbatim_hint(path: &Path) -> &'static str {
    if is_verbatim(path) {
        " (путь в verbatim-форме \\\\?\\ — вероятен фильтр-стек; \
         рабочий путь должен быть без префикса \\\\?\\)"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_disk_prefix() {
        assert_eq!(simplify(Path::new(r"\\?\C:\Repo\src")), PathBuf::from(r"C:\Repo\src"));
    }

    #[test]
    fn keeps_unc_network_shape() {
        // Сетевой путь не должен превратиться в локальный или потерять хост.
        assert_eq!(
            simplify(Path::new(r"\\?\UNC\srv\share\proj")),
            PathBuf::from(r"\\srv\share\proj")
        );
    }

    #[test]
    fn leaves_plain_and_unc_paths_untouched() {
        assert_eq!(simplify(Path::new(r"C:\Repo")), PathBuf::from(r"C:\Repo"));
        assert_eq!(simplify(Path::new(r"\\srv\share\x")), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(simplify(Path::new("/usr/src")), PathBuf::from("/usr/src"));
    }

    #[test]
    fn leaves_unknown_verbatim_namespace_untouched() {
        // `\\?\Volume{…}` — не диск и не UNC; обычной формы у нас нет, не трогаем.
        let vol = Path::new(r"\\?\Volume{9f0f}\proj");
        assert_eq!(simplify(vol), vol);
    }

    #[test]
    fn canonicalize_of_existing_path_has_no_prefix() {
        let dir = std::env::temp_dir();
        let canon = canonicalize(&dir);
        assert!(!is_verbatim(&canon), "канонический путь не должен быть verbatim: {}", canon.display());
        assert!(!canon.as_os_str().is_empty());
    }

    #[test]
    fn hint_only_for_verbatim() {
        assert!(verbatim_hint(Path::new(r"\\?\C:\x")).contains("verbatim"));
        assert!(verbatim_hint(Path::new(r"C:\x")).is_empty());
    }
}
