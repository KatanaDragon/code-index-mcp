//! Полный сбор слоя метаданных: перечень объектов, связи данных, права
//! ролей, формы, подписки, обращения в коде, механические термы.

use std::path::Path;
use anyhow::Result;
use rusqlite::params;
use walkdir::WalkDir;
use crate::xml::config_dump_info::{
    parse_config_dump_info_id_map, parse_config_dump_info_rows,
};
use crate::xml::configuration::parse_configuration_file;
use crate::xml::event_subscriptions::parse_event_subscription_file;
use crate::xml::dcs::{is_dcs_template, parse_dcs_queries};
use crate::xml::forms::{parse_form_file, parse_form_refs_file, FormRefs};
use crate::code_usages::{extract_code_usages, extract_query_usages};
use crate::xml::metadata_refs::{
    parse_defined_type_targets_file, parse_exchange_plan_content_file,
    parse_functional_option_content_file, parse_functional_option_location_file,
    parse_role_rights_file, parse_subsystem_content_file,
};
use crate::xml::object_attributes::{
    parse_object_attributes_file, parse_object_header_xml,
    parse_object_structure_file, parse_template_type, ObjectStructure,
};

use super::*;


/// Наполнить реестр `config_manifest` строками ConfigDumpInfo.xml всех
/// областей выгрузки (base + каждое расширение). Полный DELETE repo +
/// reinsert — идемпотентно, как остальные фазы слоя метаданных. `area`
/// вычисляется той же `compute_extension_name`, что и
/// `metadata_objects.sub_config` (дом объекта), поэтому Фаза 2 отличит
/// пропажу строки у дома (реальное удаление) от пропажи у заимствователя.
/// Дёшево: читает только текст описей, объектные XML не трогает.
pub(crate) fn index_config_manifest(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let roots = sub_config_roots(repo_root); // base-first

    // Защита от cascade-ошибки (см. index_metadata_objects): закрыть
    // возможную открытую транзакцию предыдущей фазы перед своей.
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM config_manifest WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO config_manifest (repo, area, full_name, config_version) \
         VALUES (?, ?, ?, ?)",
    )?;
    let mut total = 0usize;
    for sub_root in &roots {
        let area = compute_extension_name(repo_root, sub_root);
        let rows = match parse_config_dump_info_rows(sub_root) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("config_manifest parse {}: {}", sub_root.display(), e);
                continue;
            }
        };
        for (full_name, config_version) in rows {
            stmt.execute(params![REPO_DEFAULT, &area, &full_name, &config_version])?;
            total += 1;
        }
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(format!(
        "{} описи",
        code_index_core::logging::plural(total as u64, "строка", "строки", "строк")
    ));
    tracing::info!(
        "config_manifest: записано {} строк описи из {} областей",
        total,
        roots.len(),
    );
    Ok(())
}


/// Общая карта «идентификатор объекта → полное имя» по описям всех областей.
/// Заимствованный объект в составе подсистемы расширения указан идентификатором,
/// а сам объект живёт в базовой конфигурации — поэтому карта общая, а не по
/// одной области. Нечитаемая опись области пропускается с записью в журнал.
pub(crate) fn build_id_map(roots: &[std::path::PathBuf]) -> std::collections::HashMap<String, String> {
    let mut out: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for root in roots {
        match parse_config_dump_info_id_map(root) {
            Ok(m) => out.extend(m),
            Err(e) => tracing::warn!(
                "ConfigDumpInfo {}: {} — идентификаторы области не развернуты",
                root.display(),
                e
            ),
        }
    }
    out
}


pub(crate) fn index_metadata_objects(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Сначала собираем все Configuration.xml в репо (multi-config layout):
    //   * <root>/Configuration.xml — классическая выгрузка одной конфигурации;
    //   * <root>/<sub>/Configuration.xml — типичный git-репо с base/ + extensions/<EF_X>/;
    //   * глубина ограничена 3 уровнями (см. processor::detects()).
    //
    // Для каждого Configuration.xml парсим объекты и пишем в общий
    // `metadata_objects` (UNIQUE по `(repo, full_name)`, INSERT OR IGNORE
    // — заимствованные в расширениях объекты с тем же full_name просто
    // пропускаются, в выдаче остаётся base-версия).
    let mut config_paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in WalkDir::new(repo_root).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some("Configuration.xml")
        {
            config_paths.push(entry.path().to_path_buf());
        }
    }

    if config_paths.is_empty() {
        return Ok(());
    }

    // Защита от cascade-ошибки: если предыдущая функция оставила
    // открытую транзакцию (например, упала между BEGIN и COMMIT),
    // SQLite ругнётся «cannot start a transaction within a transaction».
    // Идемпотентный ROLLBACK закрывает её без ошибок если она была.
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    // Идемпотентность: при повторном run_index_extras очищаем все
    // прежние объекты репо — иначе при удалении расширения старые
    // записи остались бы навсегда.
    conn.execute(
        "DELETE FROM metadata_objects WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO metadata_objects (repo, full_name, meta_type, name) \
         VALUES (?, ?, ?, ?)",
    )?;
    let mut total = 0usize;
    let mut sources: Vec<(String, usize)> = Vec::with_capacity(config_paths.len());
    for cfg_path in &config_paths {
        let objects = match parse_configuration_file(cfg_path) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("parse_configuration_file({}): {}", cfg_path.display(), e);
                continue;
            }
        };
        let count_before = total;
        for obj in &objects {
            stmt.execute(params![
                REPO_DEFAULT,
                &obj.full_name,
                &obj.meta_type,
                &obj.name,
            ])?;
            total += 1;
        }
        sources.push((cfg_path.display().to_string(), total - count_before));
    }

    // В Configuration.xml перечислены только подсистемы верхнего уровня.
    // Вложенные лежат деревом `Subsystems/<Родитель>/Subsystems/<Ребёнок>.xml`
    // и раньше в реестр не попадали: состав их разбирался, рёбра писались, а
    // сам объект найти было нельзя (get_object_structure отвечал «не найден»).
    let mut nested_subsystems = 0usize;
    for cfg_path in &config_paths {
        let root = match cfg_path.parent() {
            Some(p) => p,
            None => continue,
        };
        let sub_dir = root.join("Subsystems");
        if !sub_dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&sub_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("xml") {
                continue;
            }
            // Только файлы-определения подсистем (Ext/Forms и прочее — мимо).
            if path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str())
                != Some("Subsystems")
            {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            // Верхнеуровневые уже вставлены из Configuration.xml — IGNORE вернёт 0.
            nested_subsystems += stmt.execute(params![
                REPO_DEFAULT,
                &format!("Subsystem.{}", stem),
                "Subsystem",
                stem,
            ])?;
        }
    }
    total += nested_subsystems;

    drop(stmt);
    crate::schema::backfill_metadata_object_keys(conn)?;
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "объект",
        "объекта",
        "объектов",
    ));
    tracing::info!(
        "metadata_objects: записано {} объектов из {} Configuration.xml (вложенных подсистем {})",
        total,
        config_paths.len(),
        nested_subsystems,
    );
    for (src, n) in sources {
        tracing::debug!("  {} → {} объектов", src, n);
    }
    Ok(())
}


/// Заполнить `data_links` — граф связей данных конфигурации.
///
/// Для каждой sub-config обходит папки объектов со ссылочными реквизитами
/// (`OBJECT_FOLDERS`), открывает корневой XML каждого объекта
/// (`Catalogs/<Имя>.xml`) и через `parse_object_attributes_file` извлекает
/// рёбра «объект → объект» по ссылочным типам реквизитов/измерений.
///
/// Полный пересбор (DELETE+INSERT всего репо) — идемпотентно, как остальной
/// `index_extras`. Объём IO невелик (для УТ ~1900 XML / ~68 МБ, ~1-3 сек),
/// поэтому инкрементальность здесь не нужна.
pub(crate) fn index_data_links(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Корни sub-config — родители найденных Configuration.xml.
    let mut sub_roots: Vec<std::path::PathBuf> = Vec::new();
    for entry in WalkDir::new(repo_root).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some("Configuration.xml")
        {
            if let Some(parent) = entry.path().parent() {
                sub_roots.push(parent.to_path_buf());
            }
        }
    }
    if sub_roots.is_empty() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute("DELETE FROM data_links WHERE repo = ?", params![REPO_DEFAULT])?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO data_links \
         (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    let mut objects: usize = 0;
    for sub_root in &sub_roots {
        for (folder, meta_type) in OBJECT_FOLDERS {
            let dir = sub_root.join(folder);
            if !dir.is_dir() {
                continue;
            }
            // Только файлы верхнего уровня (Catalogs/<Имя>.xml), не подпапки
            // (Catalogs/<Имя>/Forms/... — это формы, не структура объекта).
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("data_links: read_dir({}): {}", dir.display(), e);
                    continue;
                }
            };
            for entry in read.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_file() || path.extension().and_then(|x| x.to_str()) != Some("xml") {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let owner_full = format!("{}.{}", meta_type, stem);
                let edges = match parse_object_attributes_file(&path, &owner_full) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("data_links: {}: {}", path.display(), e);
                        continue;
                    }
                };
                objects += 1;
                for edge in edges {
                    stmt.execute(params![
                        REPO_DEFAULT,
                        &owner_full,
                        &edge.from_path,
                        &edge.to_object,
                        edge.link_kind,
                        edge.is_composite as i64,
                        edge.is_universal as i64,
                    ])?;
                    total += 1;
                }
            }
        }
    }
    drop(stmt);
    backfill_data_link_keys(conn)?;
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "ребро",
        "ребра",
        "рёбер",
    ));
    tracing::info!(
        "data_links: {} рёбер из {} объектов ({} sub-config)",
        total,
        objects,
        sub_roots.len()
    );
    Ok(())
}


/// Заполнить рёбра `data_links` КОНФИГУРАЦИОННОГО уровня (этап 3.1):
/// `subsystem_content`, `exchange_plan_content`, `defined_type_content`,
/// `functional_option_location`. Источники — отдельные XML, которые
/// `index_data_links` не читает (Subsystems/**, ExchangePlans/<X>/Ext/Content.xml,
/// DefinedTypes/<X>.xml, FunctionalOptions/<X>.xml).
///
/// ВАЖНО: вызывать ПОСЛЕ `index_data_links` — она wipe-ит все рёбра repo и
/// пишет объектные. Эта функция сносит только СВОИ `link_kind` (идемпотентность
/// + корректность инкрементального пути, где `index_data_links` не вызывается).
pub(crate) fn index_metadata_refs(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let roots = sub_config_roots(repo_root);
    if roots.is_empty() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ?1 AND link_kind IN \
         ('subsystem_content','exchange_plan_content','defined_type_content',\
          'functional_option_location','functional_option_content')",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO data_links \
         (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    // Карта «идентификатор → имя объекта» из описей выгрузки. Собирается лениво:
    // нужна только если в составе подсистемы встретился идентификатор вместо
    // имени, а это бывает лишь в выгрузках расширений.
    let mut id_map: Option<std::collections::HashMap<String, String>> = None;
    for root in &roots {
        // ── Подсистемы: Subsystems/**.xml ──────────────────────────────────
        // Файл-определение подсистемы лежит прямо в папке "Subsystems"
        // (вложенные — в <Parent>/Subsystems/<Child>.xml). Ext/Forms — пропуск.
        let sub_dir = root.join("Subsystems");
        if sub_dir.is_dir() {
            for entry in WalkDir::new(&sub_dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("xml") {
                    continue;
                }
                if path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str())
                    != Some("Subsystems")
                {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let from_object = format!("Subsystem.{}", stem);
                match parse_subsystem_content_file(path) {
                    Ok(items) => {
                        for to_object in items {
                            // В выгрузке расширения заимствованный объект в
                            // составе подсистемы указан идентификатором, а не
                            // именем. Разворачиваем его по описи выгрузки;
                            // неразвёрнутый идентификатор — не ребро, а мусор.
                            let to_object = if to_object.contains('.') {
                                to_object
                            } else {
                                let map = id_map.get_or_insert_with(|| build_id_map(&roots));
                                match map.get(&to_object) {
                                    Some(name) => name.clone(),
                                    None => {
                                        tracing::debug!(
                                            "subsystem_content {}: идентификатор {} не найден в описи",
                                            from_object,
                                            to_object
                                        );
                                        continue;
                                    }
                                }
                            };
                            stmt.execute(params![
                                REPO_DEFAULT,
                                &from_object,
                                "",
                                &to_object,
                                "subsystem_content",
                                0_i64,
                                0_i64
                            ])?;
                            total += 1;
                        }
                    }
                    Err(e) => tracing::warn!("subsystem_content {}: {}", path.display(), e),
                }
            }
        }

        // ── Планы обмена: ExchangePlans/<Имя>/Ext/Content.xml ───────────────
        let ep_dir = root.join("ExchangePlans");
        if ep_dir.is_dir() {
            for entry in WalkDir::new(&ep_dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file()
                    || entry.file_name().to_str() != Some("Content.xml")
                {
                    continue;
                }
                let path = entry.path();
                // <Имя> = папка на два уровня выше (…/<Имя>/Ext/Content.xml).
                let name = path
                    .parent()
                    .and_then(|ext| ext.parent())
                    .and_then(|d| d.file_name())
                    .and_then(|s| s.to_str());
                let name = match name {
                    Some(n) => n,
                    None => continue,
                };
                let from_object = format!("ExchangePlan.{}", name);
                match parse_exchange_plan_content_file(path) {
                    Ok(items) => {
                        for to_object in items {
                            stmt.execute(params![
                                REPO_DEFAULT,
                                &from_object,
                                "",
                                &to_object,
                                "exchange_plan_content",
                                0_i64,
                                0_i64
                            ])?;
                            total += 1;
                        }
                    }
                    Err(e) => tracing::warn!("exchange_plan_content {}: {}", path.display(), e),
                }
            }
        }

        // ── Определяемые типы: DefinedTypes/<Имя>.xml ───────────────────────
        let dt_dir = root.join("DefinedTypes");
        if dt_dir.is_dir() {
            if let Ok(read) = std::fs::read_dir(&dt_dir) {
                for entry in read.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if !path.is_file()
                        || path.extension().and_then(|e| e.to_str()) != Some("xml")
                    {
                        continue;
                    }
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s,
                        None => continue,
                    };
                    let from_object = format!("DefinedType.{}", stem);
                    match parse_defined_type_targets_file(&path) {
                        Ok(targets) => {
                            let is_composite = targets.len() > 1;
                            for (to_object, is_universal) in targets {
                                stmt.execute(params![
                                    REPO_DEFAULT,
                                    &from_object,
                                    "",
                                    &to_object,
                                    "defined_type_content",
                                    is_composite as i64,
                                    is_universal as i64
                                ])?;
                                total += 1;
                            }
                        }
                        Err(e) => tracing::warn!("defined_type_content {}: {}", path.display(), e),
                    }
                }
            }
        }

        // ── Функциональные опции: FunctionalOptions/<Имя>.xml (<Location>) ──
        let fo_dir = root.join("FunctionalOptions");
        if fo_dir.is_dir() {
            if let Ok(read) = std::fs::read_dir(&fo_dir) {
                for entry in read.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if !path.is_file()
                        || path.extension().and_then(|e| e.to_str()) != Some("xml")
                    {
                        continue;
                    }
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s,
                        None => continue,
                    };
                    let from_object = format!("FunctionalOption.{}", stem);
                    match parse_functional_option_location_file(&path) {
                        Ok(Some((to_object, raw_location))) => {
                            stmt.execute(params![
                                REPO_DEFAULT,
                                &from_object,
                                &raw_location,
                                &to_object,
                                "functional_option_location",
                                0_i64,
                                0_i64
                            ])?;
                            total += 1;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!("functional_option_location {}: {}", path.display(), e)
                        }
                    }
                    // W1: состав опции (<Content>) → рёбра functional_option_content
                    // (ФО → включаемый объект/реквизит).
                    match parse_functional_option_content_file(&path) {
                        Ok(items) => {
                            for to_object in items {
                                stmt.execute(params![
                                    REPO_DEFAULT,
                                    &from_object,
                                    "",
                                    &to_object,
                                    "functional_option_content",
                                    0_i64,
                                    0_i64
                                ])?;
                                total += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("functional_option_content {}: {}", path.display(), e)
                        }
                    }
                }
            }
        }
    }
    drop(stmt);
    backfill_data_link_keys(conn)?;
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "ребро",
        "ребра",
        "рёбер",
    ));
    tracing::info!(
        "data_links(config-level): {} рёбер ({} sub-config)",
        total,
        roots.len()
    );
    Ok(())
}


/// Заполнить `role_rights` из `Roles/<Имя>/Ext/Rights.xml` по всем sub-config.
/// Полный wipe+rebuild одной таблицы — идемпотентно. Хранятся только granted-
/// права (`<value>true</value>`). Имя роли = папка на два уровня выше Rights.xml.
pub(crate) fn index_role_rights(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let roots = sub_config_roots(repo_root);
    if roots.is_empty() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute("DELETE FROM role_rights WHERE repo = ?", params![REPO_DEFAULT])?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO role_rights (repo, role_name, object_name, right_name) \
         VALUES (?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    let mut roles: usize = 0;
    for root in &roots {
        let roles_dir = root.join("Roles");
        if !roles_dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&roles_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() || entry.file_name().to_str() != Some("Rights.xml") {
                continue;
            }
            let path = entry.path();
            let role_name = path
                .parent()
                .and_then(|ext| ext.parent())
                .and_then(|d| d.file_name())
                .and_then(|s| s.to_str());
            let role_name = match role_name {
                Some(n) => n,
                None => continue,
            };
            match parse_role_rights_file(path) {
                Ok(rights) => {
                    roles += 1;
                    for r in rights {
                        stmt.execute(params![
                            REPO_DEFAULT,
                            role_name,
                            &r.object_name,
                            &r.right_name
                        ])?;
                        total += 1;
                    }
                }
                Err(e) => tracing::warn!("role_rights {}: {}", path.display(), e),
            }
        }
    }
    drop(stmt);
    backfill_role_right_keys(conn)?;
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "право",
        "права",
        "прав",
    ));
    tracing::info!(
        "role_rights: {} прав из {} ролей ({} sub-config)",
        total,
        roles,
        roots.len()
    );
    Ok(())
}


/// Заполнить `metadata_code_usages` (этап 3.2): обратный индекс использований
/// объектов МД в коде. Проходит ВСЕ `.bsl` репо, извлекает обращения лёгким
/// regex-слоем (`extract_code_usages`). Полный пересбор (DELETE по repo +
/// INSERT) — идемпотентно. Чтение .bsl с диска (как core-индексатор); файлы не
/// в UTF-8 пропускаются.
pub(crate) fn index_metadata_code_usages(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    // Только виды из `.bsl`: обращения из макетов СКД (`dcs_query`) и форм
    // (`form_query`) ведут свои фазы XML-слоя и сносят свои строки сами.
    conn.execute(
        "DELETE FROM metadata_code_usages WHERE repo = ? \
         AND usage_kind IN ('manager', 'ref_type', 'query')",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO metadata_code_usages \
         (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    let mut files: usize = 0;
    for entry in WalkDir::new(repo_root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_bsl = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("bsl"))
            == Some(true);
        if !is_bsl {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // не UTF-8 / нечитаемый — пропуск
        };
        let usages = extract_code_usages(&content);
        if usages.is_empty() {
            continue;
        }
        let rel = rel_path(repo_root, path);
        files += 1;
        for u in usages {
            stmt.execute(params![
                REPO_DEFAULT,
                &u.object_ref,
                &u.object_ref_key,
                &u.member_path,
                u.usage_kind,
                &rel,
                u.line as i64,
            ])?;
            total += 1;
        }
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "обращение",
        "обращения",
        "обращений",
    ));
    tracing::info!(
        "metadata_code_usages: {} обращений из {} .bsl",
        total,
        files
    );
    Ok(())
}


/// Заполнить `metadata_objects.attributes_json` полной структурой объектов.
///
/// Для КАЖДОГО объекта структура аккумулируется по ВСЕМ sub-config'ам (base +
/// расширения) и мерджится (base-first, см. `ObjectStructure::merge_from`) —
/// иначе последняя обработанная sub-config затирала бы базовую структуру (баг
/// до 0.21.0: тяжёлый документ с 145 реквизитами получал 1 реквизит из
/// расширения). Затем UPDATE строки `metadata_objects` по `full_name` (строки
/// уже созданы `index_metadata_objects`). Объекты без структуры остаются с
/// `attributes_json = NULL`.
pub(crate) fn index_object_attributes(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let sub_roots = sub_config_roots(repo_root);
    if sub_roots.is_empty() {
        return Ok(());
    }

    // Аккумулируем структуру каждого объекта по всем sub-config'ам. Каждый XML
    // парсится один раз; merge_from добавляет только новые поля расширений.
    let mut acc: std::collections::HashMap<String, ObjectStructure> =
        std::collections::HashMap::new();
    for sub_root in &sub_roots {
        for (folder, meta_type) in OBJECT_FOLDERS {
            let dir = sub_root.join(folder);
            if !dir.is_dir() {
                continue;
            }
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("object_attributes: read_dir({}): {}", dir.display(), e);
                    continue;
                }
            };
            for entry in read.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_file() || path.extension().and_then(|x| x.to_str()) != Some("xml") {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let structure = match parse_object_structure_file(&path) {
                    Ok(Some(s)) => s,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("object_attributes: {}: {}", path.display(), e);
                        continue;
                    }
                };
                if structure.is_empty() {
                    continue;
                }
                let full_name = format!("{}.{}", meta_type, stem);
                match acc.get_mut(&full_name) {
                    Some(existing) => existing.merge_from(&structure),
                    None => {
                        acc.insert(full_name, structure);
                    }
                }
            }
        }
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    let mut stmt = conn.prepare(
        "UPDATE metadata_objects SET attributes_json = ? WHERE repo = ? AND full_name = ?",
    )?;
    let mut filled: usize = 0;
    for (full_name, structure) in &acc {
        if structure.is_empty() {
            continue;
        }
        stmt.execute(params![
            structure.to_json().to_string(),
            REPO_DEFAULT,
            full_name,
        ])?;
        filled += 1;
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        filled as u64,
        "объект",
        "объекта",
        "объектов",
    ));
    tracing::info!(
        "object_attributes: заполнено attributes_json у {} объектов ({} sub-config, base-first merge)",
        filled,
        sub_roots.len()
    );
    Ok(())
}


/// Заполнить `metadata_objects.synonym` для ВСЕХ объектов (вариант B): отдельный
/// лёгкий проход по корневым XML всех папок типов в каждой sub-config. В отличие
/// от `index_object_attributes` (только OBJECT_FOLDERS — объекты со структурой),
/// покрывает и CommonModule/Constant/CommonPicture/FunctionalOption/… Берёт лишь
/// шапку (meta_type/name/synonym) — `parse_object_header_xml` прерывается на
/// `<ChildObjects>`, поэтому дёшев. UPDATE по full_name: записи уже созданы
/// `index_metadata_objects`; для отсутствующих UPDATE — no-op. base-приоритет
/// (sub_roots: base первым → его synonym не перетирается расширением).
pub(crate) fn index_object_synonyms(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let sub_roots = sub_config_roots(repo_root);
    if sub_roots.is_empty() {
        return Ok(());
    }
    let mut syn: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for sub_root in &sub_roots {
        let type_dirs = match std::fs::read_dir(sub_root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for td in type_dirs.filter_map(|e| e.ok()) {
            let tdir = td.path();
            if !tdir.is_dir() {
                continue;
            }
            let files = match std::fs::read_dir(&tdir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for f in files.filter_map(|e| e.ok()) {
                let p = f.path();
                if !p.is_file() || p.extension().and_then(|x| x.to_str()) != Some("xml") {
                    continue;
                }
                let content = match std::fs::read_to_string(&p) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Some((mt, nm, Some(s))) = parse_object_header_xml(&content) {
                    if !s.is_empty() {
                        syn.entry(format!("{}.{}", mt, nm)).or_insert(s);
                    }
                }
            }
        }
        // Вложенные подсистемы лежат на два уровня глубже
        // (`Subsystems/<Родитель>/Subsystems/<Ребёнок>.xml`) и под условие «файл
        // прямо внутри папки типа» не подходят — их шапка не читалась вовсе, и
        // синоним оставался пустым у подавляющего большинства подсистем (E-6).
        // Обходим папку подсистем деревом, как это уже делает заведение перечня
        // объектов. Верхнеуровневые повторно не перетираются: `or_insert`.
        let subsystems_dir = sub_root.join("Subsystems");
        if subsystems_dir.is_dir() {
            for entry in WalkDir::new(&subsystems_dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let p = entry.path();
                if p.extension().and_then(|x| x.to_str()) != Some("xml") {
                    continue;
                }
                // Только файлы-определения подсистем (Ext/Forms и прочее — мимо).
                if p.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str())
                    != Some("Subsystems")
                {
                    continue;
                }
                let content = match std::fs::read_to_string(p) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Some((mt, nm, Some(s))) = parse_object_header_xml(&content) {
                    if !s.is_empty() {
                        syn.entry(format!("{}.{}", mt, nm)).or_insert(s);
                    }
                }
            }
        }
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    let mut stmt = conn.prepare(
        "UPDATE metadata_objects SET synonym = ? WHERE repo = ? AND full_name = ?",
    )?;
    let mut filled = 0usize;
    for (full_name, synonym) in &syn {
        filled += stmt.execute(params![synonym, REPO_DEFAULT, full_name])?;
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        filled as u64,
        "объект",
        "объекта",
        "объектов",
    ));
    tracing::info!("object_synonyms: заполнен synonym у {} объектов", filled);
    Ok(())
}


/// Полный проход механического обогащения термов (без LLM): по каждому `.bsl`
/// репо разобрать шапку модуля и описания процедур, записать шапку в
/// `module_enrichment`, а термы процедур — в `procedure_enrichment` с подписью
/// `MECH_SIGNATURE`. Строки с ДРУГОЙ подписью (LLM-enrich) не трогаются: свои
/// строки предварительно сносятся, вставка — `ON CONFLICT DO NOTHING`. Тексты
/// читаются с диска (один read на файл). См. `crate::terms`.
pub(crate) fn index_procedure_terms(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let filled = rebuild_procedure_terms_with(conn, |rel| {
        std::fs::read_to_string(repo_root.join(rel.replace('\\', "/")))
            .map(|c| c.lines().map(String::from).collect())
            .unwrap_or_default()
    })?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        filled as u64,
        "процедура",
        "процедуры",
        "процедур",
    ));
    tracing::info!("procedure_terms: механически обогащено {} процедур", filled);
    Ok(())
}


/// Тот же терм-проход, но текст модулей берётся не с диска, а из сохранённого
/// в базе содержимого файлов (`file_contents`, zstd). Нужен миграции
/// `migrate_extensions`: при смене подписи термов пересобрать надо ТОЛЬКО слой
/// термов, корня репо у миграции нет, а второй обход диска ей не нужен.
/// Возвращает число записанных процедур.
pub(crate) fn rebuild_procedure_terms_from_db(conn: &rusqlite::Connection) -> Result<usize> {
    let mut missing: usize = 0;
    let filled = rebuild_procedure_terms_with(conn, |rel| {
        match read_module_lines_from_db(conn, rel) {
            Some(lines) => lines,
            None => {
                // Текста нет — oversize-файл или индекс, собранный до появления
                // file_contents. Термы соберутся без описания (имя + объект +
                // синоним), полный текст вернёт следующая индексация с диска.
                missing += 1;
                Vec::new()
            }
        }
    })?;
    if missing > 0 {
        tracing::warn!(
            "термы процедур: у {} модулей в базе нет сохранённого текста \
             (oversize или индекс старой версии) — описания и шапки у них не восстановлены",
            missing
        );
    }
    Ok(filled)
}


/// Разжать текст `.bsl` из `file_contents` по repo-relative пути.
/// `None` — записи нет, файл oversize или blob повреждён.
fn read_module_lines_from_db(conn: &rusqlite::Connection, rel: &str) -> Option<Vec<String>> {
    use rusqlite::OptionalExtension;
    use std::io::Read;

    /// Потолок разжатого модуля — как у core (защита от zstd-bomb в
    /// повреждённой базе); любой реальный .bsl многократно меньше.
    const MAX_DECOMPRESSED: u64 = 256 * 1024 * 1024;

    let blob: Vec<u8> = conn
        .query_row(
            "SELECT fc.content_blob FROM file_contents fc \
             JOIN files f ON f.id = fc.file_id WHERE f.path = ?1",
            params![rel],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten()?;
    let mut decoder = zstd::stream::read::Decoder::new(&blob[..]).ok()?;
    let mut out = Vec::new();
    let read = (&mut decoder).take(MAX_DECOMPRESSED + 1).read_to_end(&mut out).ok()?;
    if read as u64 > MAX_DECOMPRESSED {
        return None;
    }
    let text = String::from_utf8(out).ok()?;
    Some(text.lines().map(String::from).collect())
}


/// Массовая вставка термов со снятыми FTS-триггерами: триграммный токенайзер
/// срабатывал бы построчно на сотнях тысяч строк (доминирующая стоимость
/// слоя), а один `rebuild` перестраивает индекс целиком за проход. Триггеры
/// возвращаются ВСЕГДА, даже если тело упало, — иначе инкрементальный путь
/// после сброса на диск потеряет синхронизацию FTS.
fn with_fts_bulk<T>(
    conn: &rusqlite::Connection,
    body: impl FnOnce() -> Result<T>,
) -> Result<T> {
    conn.execute_batch(crate::schema::PE_FTS_TRIGGERS_DROP)?;
    let result = body();
    let rebuilt = conn
        .execute_batch("INSERT INTO fts_procedure_enrichment(fts_procedure_enrichment) VALUES('rebuild');")
        .map_err(anyhow::Error::from);
    let recreated = conn
        .execute_batch(crate::schema::PE_FTS_TRIGGERS_DDL)
        .map_err(anyhow::Error::from);
    // Ошибка тела важнее ошибок восстановления, поэтому она идёт первой.
    result.and_then(|v| rebuilt.and(recreated).map(|_| v))
}


/// Общий проход сборки механических термов: пройти все `.bsl` репо, разобрать
/// шапку модуля и описания процедур, переписать `module_enrichment` и свои
/// (`mech:%`) строки `procedure_enrichment`. Текст файла даёт `lines_of` —
/// диск при индексации, `file_contents` при терм-проходе миграции.
/// Возвращает число записанных процедур.
fn rebuild_procedure_terms_with<F>(conn: &rusqlite::Connection, mut lines_of: F) -> Result<usize>
where
    F: FnMut(&str) -> Vec<String>,
{
    use crate::terms::{
        build_terms, extract_leading_comment, extract_module_header, object_from_module_path,
        MECH_SIGNATURE,
    };

    // Синонимы объектов: full_name → synonym (один SELECT на репо).
    let mut syn: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT full_name, synonym FROM metadata_objects \
             WHERE repo = ?1 AND synonym IS NOT NULL AND synonym != ''",
        )?;
        let rows = stmt.query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows.flatten() {
            syn.insert(row.0, row.1);
        }
    }

    // Процедуры, сгруппированные по файлу. Обход ведётся по файлам, а не по
    // процедурам: шапка есть и у модуля без единой процедуры.
    let mut procs_by_path: std::collections::HashMap<String, Vec<(String, i64)>> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT fl.path, f.name, COALESCE(f.line_start, 0) FROM functions f \
             JOIN files fl ON fl.id = f.file_id \
             WHERE fl.path LIKE '%.bsl' ORDER BY fl.path, f.line_start",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        for (path, name, line_start) in rows.flatten() {
            procs_by_path.entry(path).or_default().push((name, line_start));
        }
    }

    let files: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT path FROM files WHERE path LIKE '%.bsl' ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.flatten().collect()
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    with_fts_bulk(conn, || {
        let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
        conn.execute("BEGIN", [])?;
        conn.execute(
            "DELETE FROM procedure_enrichment WHERE repo = ?1 AND signature LIKE 'mech:%'",
            params![REPO_DEFAULT],
        )?;
        // Шапки модулей заполняет только механика — своей подписи для
        // выборочного удаления не требуется.
        conn.execute("DELETE FROM module_enrichment WHERE repo = ?1", params![REPO_DEFAULT])?;
        let mut filled = 0usize;
        {
            let mut ins_proc = conn.prepare(
                "INSERT INTO procedure_enrichment \
                 (repo, proc_key, terms, tags, comment_head, comment_len, signature, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(repo, proc_key) DO NOTHING",
            )?;
            let mut ins_mod = conn.prepare(
                "INSERT INTO module_enrichment \
                 (repo, path, header, tags, depends, signature, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(repo, path) DO UPDATE SET \
                   header = excluded.header, tags = excluded.tags, \
                   depends = excluded.depends, signature = excluded.signature, \
                   updated_at = excluded.updated_at",
            )?;
            for path in &files {
                let procs = procs_by_path.get(path);
                let lines = lines_of(path);
                let header = extract_module_header(&lines);
                let module_tags: Vec<String> =
                    header.as_ref().map(|h| h.tags.clone()).unwrap_or_default();
                if let Some(h) = &header {
                    ins_mod.execute(params![
                        REPO_DEFAULT,
                        path,
                        &h.prose,
                        h.tags_line(),
                        h.depends_line(),
                        MECH_SIGNATURE,
                        now
                    ])?;
                }
                let Some(procs) = procs else { continue };
                let object = object_from_module_path(path);
                let synonym = object
                    .as_ref()
                    .and_then(|(mt, nm)| syn.get(&format!("{}.{}", mt, nm)))
                    .map(String::as_str);
                for (name, line_start) in procs {
                    let comment = extract_leading_comment(&lines, (*line_start).max(0) as usize);
                    let terms = build_terms(
                        name,
                        object.as_ref().map(|(_, nm)| nm.as_str()),
                        synonym,
                        comment.as_ref(),
                        &module_tags,
                    );
                    if terms.is_empty() {
                        continue;
                    }
                    let proc_key = format!("{}::{}", path, name);
                    filled += ins_proc.execute(params![
                        REPO_DEFAULT,
                        proc_key,
                        terms,
                        comment.as_ref().map(|c| c.tags_line()).unwrap_or_default(),
                        comment.as_ref().map(|c| c.head.clone()).unwrap_or_default(),
                        comment.as_ref().map(|c| c.len_chars as i64).unwrap_or(0),
                        MECH_SIGNATURE,
                        now
                    ])?;
                }
            }
        }
        conn.execute("COMMIT", [])?;
        Ok(filled)
    })
}


/// Сборка механических термов из staging (`_proc_terms_staging` и
/// `_module_header_staging`, наполнены parse-collector'ом в фазе параллельного
/// парсинга) — БЕЗ повторного чтения .bsl с диска. Синоним объекта
/// подставляется по metadata_objects (синонимы заполнены XML-слоем, идущим ДО
/// этого шага). В конце staging дропается.
pub(crate) fn build_procedure_terms_from_staging(conn: &rusqlite::Connection) -> Result<()> {
    with_fts_bulk(conn, || {
        use crate::terms::{build_terms, LeadingComment, MECH_SIGNATURE};

        let mut syn: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT full_name, synonym FROM metadata_objects WHERE repo = ?1 AND synonym IS NOT NULL AND synonym != ''")?;
            let rows = stmt.query_map(params![REPO_DEFAULT], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows.flatten() {
                syn.insert(row.0, row.1);
            }
        }

        // Шапки модулей: путь → (проза, теги, зависимости). Теги отсюда идут
        // в термы процедур этого же файла.
        let headers: Vec<(String, String, String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT path, header, tags, depends FROM _module_header_staging",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            rows.flatten().collect()
        };
        let module_tags: std::collections::HashMap<String, Vec<String>> = headers
            .iter()
            .map(|(path, _, tags, _)| {
                let list: Vec<String> = tags
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                (path.clone(), list)
            })
            .collect();

        let staged: Vec<(String, String, Option<String>, Option<String>, String, String, String, i64)> = {
            let mut stmt = conn.prepare("SELECT proc_key, proc_name, object_meta_type, object_name, comment_prose, comment_head, comment_tags, comment_len FROM _proc_terms_staging")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })?;
            rows.flatten().collect()
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
        conn.execute("BEGIN", [])?;
        conn.execute("DELETE FROM procedure_enrichment WHERE repo = ?1 AND signature LIKE 'mech:%'", params![REPO_DEFAULT])?;
        conn.execute("DELETE FROM module_enrichment WHERE repo = ?1", params![REPO_DEFAULT])?;
        let mut filled = 0usize;
        {
            let mut ins_mod = conn.prepare(
                "INSERT INTO module_enrichment (repo, path, header, tags, depends, signature, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(repo, path) DO UPDATE SET \
                   header = excluded.header, tags = excluded.tags, depends = excluded.depends, \
                   signature = excluded.signature, updated_at = excluded.updated_at",
            )?;
            for (path, header, tags, depends) in &headers {
                ins_mod.execute(params![
                    REPO_DEFAULT, path, header, tags, depends, MECH_SIGNATURE, now
                ])?;
            }
        }
        {
            let mut ins = conn.prepare(
                "INSERT INTO procedure_enrichment \
                 (repo, proc_key, terms, tags, comment_head, comment_len, signature, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(repo, proc_key) DO NOTHING",
            )?;
            for (proc_key, proc_name, object_meta_type, object_name, prose, head, tags, len_chars)
                in &staged
            {
                let synonym = match (object_meta_type, object_name) {
                    (Some(mt), Some(nm)) => syn.get(&format!("{}.{}", mt, nm)).map(String::as_str),
                    _ => None,
                };
                // Разбор описания сделан в фазе парсинга — здесь собираем
                // структуру обратно из колонок staging (@depends в термы не
                // идёт, поэтому в staging его и нет).
                let comment = LeadingComment {
                    prose: prose.clone(),
                    head: head.clone(),
                    tags: tags
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect(),
                    depends: Vec::new(),
                    len_chars: (*len_chars).max(0) as usize,
                };
                let has_comment = *len_chars > 0;
                let path = proc_key.split("::").next().unwrap_or_default();
                let mtags = module_tags.get(path).cloned().unwrap_or_default();
                let terms = build_terms(
                    proc_name,
                    object_name.as_deref(),
                    synonym,
                    has_comment.then_some(&comment),
                    &mtags,
                );
                if terms.is_empty() {
                    continue;
                }
                filled += ins.execute(params![
                    REPO_DEFAULT,
                    proc_key,
                    terms,
                    comment.tags_line(),
                    &comment.head,
                    *len_chars,
                    MECH_SIGNATURE,
                    now
                ])?;
            }
        }
        conn.execute("COMMIT", [])?;

        conn.execute_batch(
            "DROP TABLE IF EXISTS _proc_terms_staging; DROP TABLE IF EXISTS _module_header_staging;",
        )?;

        code_index_core::logging::stage_detail(code_index_core::logging::plural(
            filled as u64,
            "процедура",
            "процедуры",
            "процедур",
        ));
        tracing::info!(
            "procedure_terms (staging): механически обогащено {} процедур, шапок модулей {}",
            filled,
            headers.len()
        );
        Ok(())
    })
}


pub(crate) fn index_metadata_forms(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Ищем `Form.xml` в любом дочернем `Forms/<Name>/[Ext/]Form.xml`.
    // Имя владельца восстанавливается из пути: ищем сегмент под
    // `Forms/`, значит путь выглядит как `<...>/<MetaType>/<OwnerName>/Forms/<FormName>/...Form.xml`.
    let mut count = 0usize;
    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_forms WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        // INSERT OR IGNORE — заимствованные формы (одинаковый owner+form_name
        // в base/ и в extensions/<EF_X>/) дают UNIQUE-конфликт; считаем
        // что приоритет за первой записью (обычно base, поскольку
        // multi-config обход начинается от корня и base/ обычно идёт раньше).
        "INSERT OR IGNORE INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
         VALUES (?, ?, ?, ?)",
    )?;

    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if file_name != "Form.xml" {
            continue;
        }
        // Path: .../<MetaType>/<OwnerName>/Forms/<FormName>/[Ext/]Form.xml
        let (owner_full, form_name) = match decode_form_path(repo_root, path) {
            Some(t) => t,
            None => continue,
        };
        let handlers = match parse_form_file(path) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("parse_form_file({}): {}", path.display(), e);
                continue;
            }
        };
        let handlers_json = crate::xml::forms::handlers_to_json(&handlers)?;
        stmt.execute(params![
            REPO_DEFAULT,
            &owner_full,
            &form_name,
            &handlers_json,
        ])?;
        count += 1;
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        count as u64,
        "форма",
        "формы",
        "форм",
    ));
    tracing::info!("metadata_forms: проиндексировано {} форм", count);
    Ok(())
}


/// Макеты, принадлежащие объектам (`<Вид>/<Объект>/Templates/<Имя>.xml`), —
/// в перечень объектов отдельными строками `<Вид>.<Объект>.Template.<Имя>`.
/// Общие макеты (папка `CommonTemplates`) сюда не попадают: они объекты
/// верхнего уровня и заводятся перечнем из оглавления конфигурации.
///
/// В перечень идёт только ПАСПОРТ макета: имя, синоним, владелец и вид
/// (табличный документ, схема компоновки…). Содержимое макета — отдельный
/// разговор: печатные формы доходят до десятков мегабайт и в индекс
/// намеренно не берутся. Чтобы отсутствие содержимого не выглядело как
/// пропажа объекта, в паспорт пишется признак `content_indexed` и размер
/// файла — по ним видно, что макет существует, а искать по его тексту
/// нельзя.
///
/// Идемпотентно: DELETE своих строк репо + INSERT. Обязана идти ПОСЛЕ
/// `index_metadata_objects` — та чистит весь перечень репо целиком.
pub(crate) fn index_object_templates(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let mut rows: Vec<TemplateRow> = Vec::new();
    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        // Описание макета лежит ПРЯМО в папке `Templates` объекта:
        // `<...>/<ПапкаВида>/<Объект>/Templates/<Имя>.xml`. Всё остальное
        // внутри (`<Имя>/Ext/Template.xml` — само содержимое) пропускаем.
        if path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) != Some("Templates") {
            continue;
        }
        if let Some(row) = template_row_from_path(repo_root, path) {
            rows.push(row);
        }
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_objects WHERE repo = ? AND meta_type = 'Template'",
        params![REPO_DEFAULT],
    )?;
    let mut count = 0usize;
    let mut without_content = 0usize;
    for row in &rows {
        let (inserted, content_indexed) = insert_template_row(conn, row)?;
        count += inserted;
        if !content_indexed {
            without_content += 1;
        }
    }
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        count as u64,
        "макет",
        "макета",
        "макетов",
    ));
    tracing::info!(
        "object_templates: заведено {} макетов объектов, из них без содержимого в индексе {}",
        count,
        without_content
    );
    Ok(())
}

/// Паспорт одного макета до записи в перечень.
pub(crate) struct TemplateRow {
    pub full_name: String,
    name: String,
    synonym: Option<String>,
    owner_full_name: String,
    template_type: Option<String>,
    content_rel: Option<String>,
    content_size: Option<u64>,
}

/// Двоичное ли содержимое макета — по расширению файла. Такие в текстовый
/// индекс не попадают в принципе, и списывать это на предел размера неверно.
fn is_binary_template_content(rel: &str) -> bool {
    let ext = rel.rsplit('.').next().unwrap_or_default().to_lowercase();
    !matches!(ext.as_str(), "xml" | "txt" | "html" | "htm" | "json" | "css" | "js")
}

/// Файл с содержимым макета: `Templates/<Имя>/Ext/Template.<чем-то>`.
///
/// Расширение зависит от вида макета и по имени не угадывается: табличный
/// документ и схема компоновки лежат в `.xml`, текстовый документ — в `.txt`,
/// двоичные данные и внешние компоненты — в своих форматах. Поэтому смотрим,
/// что реально лежит в папке, а не подставляем `.xml` всем подряд.
fn template_content_path(descriptor: &Path) -> Option<std::path::PathBuf> {
    let ext_dir = descriptor.with_extension("").join("Ext");
    let entries = std::fs::read_dir(&ext_dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.is_file()
                && p.file_stem().and_then(|s| s.to_str()) == Some("Template")
        })
}

/// Собрать паспорт макета по пути его описания. `None` — файл не читается,
/// это не описание макета либо папка вида неизвестна.
pub(crate) fn template_row_from_path(repo_root: &Path, path: &Path) -> Option<TemplateRow> {
    let owner_full_name = template_owner_from_path(path)?;
    let content = std::fs::read_to_string(path).ok()?;
    let (meta_type, name, synonym) = parse_object_header_xml(&content)?;
    if meta_type != "Template" {
        return None;
    }
    let template_type = parse_template_type(&content);
    let content_path = template_content_path(path);
    let content_size = content_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    let content_rel = content_path.as_ref().map(|p| rel_path(repo_root, p));
    Some(TemplateRow {
        full_name: format!("{}.Template.{}", owner_full_name, name),
        name,
        synonym,
        owner_full_name,
        template_type,
        content_rel,
        content_size,
    })
}

/// Записать паспорт макета в перечень. Возвращает (сколько строк добавлено,
/// проиндексировано ли содержимое). Транзакцию ведёт вызывающий.
///
/// Заимствованный расширением макет даёт то же полное имя — приоритет за
/// первой записью, как у форм и объектов.
pub(crate) fn insert_template_row(
    conn: &rusqlite::Connection,
    row: &TemplateRow,
) -> Result<(usize, bool)> {
    let content_indexed = match &row.content_rel {
        Some(rel) => conn
            .query_row("SELECT 1 FROM files WHERE path = ?1", params![rel], |_| Ok(()))
            .is_ok(),
        None => false,
    };
    let mut passport = serde_json::json!({
        "owner": row.owner_full_name,
        "content_indexed": content_indexed,
    });
    if let Some(t) = &row.template_type {
        passport["template_type"] = serde_json::json!(t);
    }
    if let Some(rel) = &row.content_rel {
        passport["content_file"] = serde_json::json!(rel);
    }
    if let Some(size) = row.content_size {
        passport["content_size_bytes"] = serde_json::json!(size);
    }
    if !content_indexed {
        // Причина называется по факту, а не общей фразой: искать по тексту
        // нельзя в обоих случаях, но «файл двоичный» и «файл слишком велик» —
        // разные ответы на вопрос «а что с этим делать».
        passport["content_note"] = serde_json::json!(match &row.content_rel {
            None => "у макета нет файла содержимого в выгрузке — есть только его описание",
            Some(rel) if is_binary_template_content(rel) =>
                "содержимое макета двоичное (внешняя компонента, двоичные данные) — \
                 в индекс не берётся; сам макет существует",
            Some(_) =>
                "содержимое макета не проиндексировано: файл превышает предел размера \
                 текстового файла (настройка max_file_size) — поиск по его тексту \
                 работать не будет, сам макет существует",
        });
    }
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO metadata_objects \
         (repo, full_name, meta_type, name, synonym, attributes_json, full_name_key) \
         VALUES (?, ?, 'Template', ?, ?, ?, ?)",
        params![
            REPO_DEFAULT,
            &row.full_name,
            &row.name,
            &row.synonym,
            &passport.to_string(),
            &row.full_name.to_lowercase(),
        ],
    )?;
    Ok((inserted, content_indexed))
}

/// Владелец макета по пути его описания: `<...>/Catalogs/Контрагенты/Templates/X.xml`
/// → `Catalog.Контрагенты`. `None`, если раскладка не похожа на выгрузку 1С
/// либо папка вида неизвестна.
pub(crate) fn template_owner_from_path(path: &Path) -> Option<String> {
    let templates_dir = path.parent()?;
    let object_dir = templates_dir.parent()?;
    let object = object_dir.file_name()?.to_str()?;
    let folder = object_dir.parent()?.file_name()?.to_str()?;
    let meta_type = ALL_OBJECT_FOLDERS
        .iter()
        .find(|(plural, _)| *plural == folder)
        .map(|(_, singular)| *singular)?;
    Some(format!("{}.{}", meta_type, object))
}


pub(crate) fn index_event_subscriptions(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Подписки на события могут быть в нескольких sub-config'ах
    // (base/EventSubscriptions/, extensions/<EF_X>/EventSubscriptions/...).
    // Обходим всё дерево рекурсивно (max_depth защищает от случайных
    // глубоко вложенных fixture-файлов, как и в index_metadata_objects).
    let mut count = 0usize;
    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM event_subscriptions WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )?;

    for entry in WalkDir::new(repo_root)
        .max_depth(4) // root/<sub>/EventSubscriptions/<file>.xml = depth 3, +запас
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        // Должен лежать внутри директории `EventSubscriptions/`.
        let in_event_subs_dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            == Some("EventSubscriptions");
        if !in_event_subs_dir {
            continue;
        }
        match parse_event_subscription_file(path) {
            Ok(Some(sub)) => {
                let sources_json = serde_json::to_string(&sub.sources)?;
                stmt.execute(params![
                    REPO_DEFAULT,
                    &sub.name,
                    &sub.event,
                    &sub.handler_module,
                    &sub.handler_proc,
                    &sources_json,
                ])?;
                count += 1;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("parse_event_subscription_file({}): {}", path.display(), e),
        }
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        count as u64,
        "подписка",
        "подписки",
        "подписок",
    ));
    tracing::info!("event_subscriptions: проиндексировано {} подписок", count);
    Ok(())
}


/// Ссылки форм на объекты (XML-слой): рёбра `data_links` видов `form_*` от
/// канонического владельца формы и обращения `form_query` из ручных запросов
/// динамических списков → `metadata_code_usages`. Полный пересбор своих строк
/// (DELETE по видам + INSERT) — идемпотентно. Строго ПОСЛЕ `index_data_links`:
/// та сносит все рёбра репо и пишет объектные.
pub(crate) fn index_form_refs(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    delete_form_refs_all(conn)?;

    let mut forms = 0usize;
    let mut edges = 0usize;
    let mut usages = 0usize;
    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) != Some("Form.xml") {
            continue;
        }
        let (owner_full, form_name) = match form_owner_canonical(repo_root, path) {
            Some(t) => t,
            None => continue,
        };
        let refs = match parse_form_refs_file(path) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("parse_form_refs_file({}): {}", path.display(), e);
                continue;
            }
        };
        if refs.edges.is_empty() && refs.queries.is_empty() {
            continue;
        }
        let rel = rel_path(repo_root, path);
        let (e, u) = insert_form_refs(conn, &owner_full, &form_name, &rel, &refs)?;
        forms += 1;
        edges += e;
        usages += u;
    }
    backfill_data_link_keys(conn)?;
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        edges as u64,
        "ребро",
        "ребра",
        "рёбер",
    ));
    tracing::info!(
        "data_links(form-level): {} рёбер и {} обращений из запросов списков ({} форм)",
        edges,
        usages,
        forms
    );
    Ok(())
}

/// Снести всё, что порождено формами: рёбра `form_*` и обращения `form_query`.
pub(crate) fn delete_form_refs_all(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM data_links WHERE repo = ?1 \
         AND link_kind IN ('form_attr', 'form_param', 'form_main_table')",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "DELETE FROM metadata_code_usages WHERE repo = ?1 AND usage_kind = 'form_query'",
        params![REPO_DEFAULT],
    )?;
    Ok(())
}

/// Записать ссылки одной формы. Рёбра — в `data_links` от канонического
/// владельца с `from_path` = `Form.<Имя>.<путь>`; запросы динамических
/// списков — в `metadata_code_usages` видом `form_query`, `file_path` — путь
/// файла формы. Возвращает (рёбер, обращений). Транзакцию ведёт вызывающий,
/// `to_object_key` дописывает он же (`backfill_data_link_keys`).
pub(crate) fn insert_form_refs(
    conn: &rusqlite::Connection,
    owner_full: &str,
    form_name: &str,
    rel: &str,
    refs: &FormRefs,
) -> Result<(usize, usize)> {
    let mut edges = 0usize;
    if !refs.edges.is_empty() {
        let mut stmt = conn.prepare(
            "INSERT OR IGNORE INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for edge in &refs.edges {
            let from_path = format!("Form.{}.{}", form_name, edge.from_path);
            edges += stmt.execute(params![
                REPO_DEFAULT,
                owner_full,
                &from_path,
                &edge.to_object,
                edge.link_kind,
                edge.is_composite as i64,
                edge.is_universal as i64,
            ])?;
        }
    }
    let mut usages = 0usize;
    if !refs.queries.is_empty() {
        let mut stmt = conn.prepare(
            "INSERT INTO metadata_code_usages \
             (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for (line, text) in &refs.queries {
            for u in extract_query_usages(text, *line, "form_query") {
                stmt.execute(params![
                    REPO_DEFAULT,
                    &u.object_ref,
                    &u.object_ref_key,
                    &u.member_path,
                    u.usage_kind,
                    rel,
                    u.line as i64,
                ])?;
                usages += 1;
            }
        }
    }
    Ok((edges, usages))
}


/// Запросы схем компоновки данных (XML-слой) → обращения `dcs_query` в
/// `metadata_code_usages`. Содержимое макета лежит в
/// `Templates/<Имя>/Ext/Template.xml` (у общих макетов — `CommonTemplates/`);
/// какого оно рода, видно по корневому тегу, поэтому сначала читаются первые
/// байты, и целиком читаются только схемы компоновки: макеты печатных форм
/// бывают по десятки мегабайт. Полный пересбор своих строк — идемпотентно.
pub(crate) fn index_dcs_query_usages(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_code_usages WHERE repo = ?1 AND usage_kind = 'dcs_query'",
        params![REPO_DEFAULT],
    )?;

    let mut schemas = 0usize;
    let mut total = 0usize;
    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        if !is_template_content_path(path) {
            continue;
        }
        let content = match read_dcs_content(path) {
            Some(c) => c,
            None => continue,
        };
        let queries = match parse_dcs_queries(&content) {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!("parse_dcs_queries({}): {}", path.display(), e);
                continue;
            }
        };
        if queries.is_empty() {
            continue;
        }
        schemas += 1;
        total += insert_dcs_usages(conn, &rel_path(repo_root, path), &queries)?;
    }
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "обращение",
        "обращения",
        "обращений",
    ));
    tracing::info!(
        "dcs_query_usages: {} обращений из {} схем компоновки",
        total,
        schemas
    );
    Ok(())
}

/// Содержимое макета — `Template.xml` внутри `Ext/` (описание макета лежит
/// уровнем выше, в `Templates/<Имя>.xml`, и разбирается отдельно).
pub(crate) fn is_template_content_path(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("Template.xml")
        && path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) == Some("Ext")
}

/// Прочитать содержимое макета, если это схема компоновки; иначе `None`.
/// Решение принимается по первым байтам, чтобы не читать целиком макеты
/// табличных документов.
pub(crate) fn read_dcs_content(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 2048];
    let n = f.read(&mut head).ok()?;
    if !is_dcs_template(&String::from_utf8_lossy(&head[..n])) {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Записать обращения из запросов одной схемы компоновки. Возвращает число
/// строк. Транзакцию ведёт вызывающий.
pub(crate) fn insert_dcs_usages(
    conn: &rusqlite::Connection,
    rel: &str,
    queries: &[(usize, String)],
) -> Result<usize> {
    let mut stmt = conn.prepare(
        "INSERT INTO metadata_code_usages \
         (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut n = 0usize;
    for (line, text) in queries {
        for u in extract_query_usages(text, *line, "dcs_query") {
            stmt.execute(params![
                REPO_DEFAULT,
                &u.object_ref,
                &u.object_ref_key,
                &u.member_path,
                u.usage_kind,
                rel,
                u.line as i64,
            ])?;
            n += 1;
        }
    }
    Ok(n)
}
