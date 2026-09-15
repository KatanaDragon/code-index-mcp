// Механическое обогащение процедур бизнес-терминами — БЕЗ LLM.
//
// Наполняет `procedure_enrichment.terms` на этапе индексации из пяти
// дешёвых источников:
//   1. метатеги `@tags` описания процедуры («@tags весы, настройки») —
//      отдельными фразами и отдельной колонкой `tags` с бОльшим весом в FTS;
//   2. слова имени процедуры (сплит CamelCase/подчёркиваний/смены алфавита):
//      «УточнитьДанныеПоШтрихкоду» → «уточнить данные по штрихкоду»;
//   3. слова имени объекта-владельца модуля (из пути файла):
//      Catalogs/Номенклатура/… → «номенклатура»;
//   4. синоним объекта-владельца из `metadata_objects.synonym`
//      («Реализация товаров и услуг») — механический мост
//      «русское представление ↔ английский идентификатор»;
//   5. проза комментария непосредственно над процедурой (строки `//…`)
//      и метатеги `@tags` ШАПКИ модуля (см. `extract_module_header`).
//
// Зачем: лексическая «спираль уточнения» — модель знает понятие по-русски,
// но не знает точного написания в коде (CamelCase, словоформа, английский
// идентификатор) и перебирает варианты regex'ом впустую. Термы + триграммный
// FTS (см. schema.rs) закрывают словоформы, подписи и большую часть
// кросс-языка детерминированно, за секунды на парсинге.
//
// Метатеги (`@tags`, `@depends`, `@layer`, `@security`, …) — грамматика
// `code-docs` из правил разметки кода 1С: строка `@<тег> <значение>` в конце
// описания. В прозу они не попадают (иначе имя тега и служебные слова
// зашумляют FTS), в термы идут только `@tags` — остальные теги дают модели
// шум вместо сигнала.
//
// LLM-проход `enrich` остаётся опциональной командой: механика помечает свои
// записи `signature = 'mech:vN'` и НЕ трогает строки с другой подписью.

/// Подпись механических записей в `procedure_enrichment.signature`.
/// Менять при изменении алгоритма построения термов — полный проход
/// перезапишет только свои строки, а `migrate_extensions` при открытии базы
/// увидит устаревшую подпись и сделает терм-проход сам.
///
/// `mech:v2` — разбор метатегов (`@tags`/`@depends`), шапка модуля и колонки
/// `tags`/`comment_head`/`comment_len` (было `mech:v1` — проза комментария
/// одной строкой, обрезанная до 240 символов вместе с тегами).
pub const MECH_SIGNATURE: &str = "mech:v2";

/// Вес колонки `tags` в BM25-ранжировании FTS относительно прозы (`terms`).
/// Тег ставит разработчик руками — это сигнал сильнее случайного совпадения
/// слова в описании, поэтому колонка весит больше. Значение подобрано на
/// замере вопросов «где реализовано…» (см. CHANGELOG).
pub const TAGS_BM25_WEIGHT: f64 = 4.0;

/// Предел прозы комментария процедуры в термах. Термы — сигнал для FTS,
/// а не полнотекст: длинное описание целиком размывает BM25 и раздувает
/// индекс. Полный текст описания всегда доступен через `read_file`.
const PROSE_LIMIT_CHARS: usize = 240;

/// Предел прозы шапки модуля в `module_enrichment.header`. Шапка — карточка
/// модуля для агента (её показывает `get_object_profile`), поэтому предел
/// больше, чем у термов процедуры.
const HEADER_PROSE_LIMIT_CHARS: usize = 1000;

/// Предел «первой строки» описания (`procedure_enrichment.comment_head`
/// и `header` в выдаче `get_object_profile`): одна строка в выдаче агенту.
const HEAD_LIMIT_CHARS: usize = 160;

/// Папки выгрузки с модулями → singular meta_type (как в
/// `metadata_objects.full_name`). Шире, чем `index_extras::OBJECT_FOLDERS`
/// (тот — только типы со структурой реквизитов): здесь все типы, у которых
/// бывают .bsl-модули.
const MODULE_FOLDERS: &[(&str, &str)] = &[
    ("Catalogs", "Catalog"),
    ("Documents", "Document"),
    ("DataProcessors", "DataProcessor"),
    ("Reports", "Report"),
    ("CommonModules", "CommonModule"),
    ("Enums", "Enum"),
    ("Constants", "Constant"),
    ("InformationRegisters", "InformationRegister"),
    ("AccumulationRegisters", "AccumulationRegister"),
    ("AccountingRegisters", "AccountingRegister"),
    ("CalculationRegisters", "CalculationRegister"),
    ("ChartsOfAccounts", "ChartOfAccounts"),
    ("ChartsOfCharacteristicTypes", "ChartOfCharacteristicTypes"),
    ("ChartsOfCalculationTypes", "ChartOfCalculationTypes"),
    ("ExchangePlans", "ExchangePlan"),
    ("BusinessProcesses", "BusinessProcess"),
    ("Tasks", "Task"),
    ("CommonForms", "CommonForm"),
    ("CommonCommands", "CommonCommand"),
    ("WebServices", "WebService"),
    ("HTTPServices", "HTTPService"),
    ("DocumentJournals", "DocumentJournal"),
    ("Sequences", "Sequence"),
    ("SettingsStorages", "SettingsStorage"),
    ("ExternalDataSources", "ExternalDataSource"),
    ("FilterCriteria", "FilterCriterion"),
];

/// Кириллическая ли буква (для границы смены алфавита).
fn is_cyr(c: char) -> bool {
    matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')
}

/// Разбить идентификатор на слова в нижнем регистре.
///
/// Границы: не-буквенно-цифровой символ (подчёркивание, точка, пробел),
/// lower→Upper («уточнитьДанные»), буква↔цифра, смена алфавита
/// (кириллица↔латиница: «ent_ДоработкаОбмен»), конец аббревиатуры
/// (UPPER UPPER lower: «XMLReader» → «xml reader»).
pub fn split_identifier(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();

    let flush = |cur: &mut String, words: &mut Vec<String>| {
        if !cur.is_empty() {
            words.push(std::mem::take(cur));
        }
    };

    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            flush(&mut cur, &mut words);
            continue;
        }
        if !cur.is_empty() {
            let prev = chars[i - 1];
            let boundary = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_alphabetic() != c.is_alphabetic())
                || (prev.is_alphabetic() && c.is_alphabetic() && is_cyr(prev) != is_cyr(c))
                || (prev.is_uppercase()
                    && c.is_uppercase()
                    && i + 1 < chars.len()
                    && chars[i + 1].is_lowercase());
            if boundary {
                flush(&mut cur, &mut words);
            }
        }
        // Ё→Е сразу при нормализации: модели пишут «расчёт», в идентификаторах
        // 1С — «Расчет». Без свёртки триграммы «чёт»/«чет» не совпадают.
        for lc in c.to_lowercase() {
            cur.push(if lc == 'ё' { 'е' } else { lc });
        }
    }
    flush(&mut cur, &mut words);
    words
}

/// Нормализация свободного текста для термов: нижний регистр + ё→е
/// (та же свёртка, что в `split_identifier`, — термы и запросы должны
/// нормализоваться одинаково).
pub fn fold_text(s: &str) -> String {
    s.to_lowercase().replace('ё', "е")
}

/// Обрезать текст до `limit` символов: по символам, а не по байтам
/// (кириллица), и по границе слова, если текст длиннее предела.
fn cut_chars(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let cut: String = s.chars().take(limit).collect();
    match cut.rfind(' ') {
        // Хвостовое слово отрезаем, только если от строки остаётся больше
        // половины предела — иначе одно длинное слово съело бы всю выдачу.
        Some(pos) if pos > limit / 2 => cut[..pos].trim_end().to_string(),
        _ => cut.trim_end().to_string(),
    }
}

/// По repo-relative пути .bsl-модуля определить объект-владельца:
/// `(meta_type, имя)` — `Catalogs/Номенклатура/Ext/ObjectModule.bsl` →
/// `("Catalog", "Номенклатура")`. Работает и для форм
/// (`…/Forms/ФормаЭлемента/Ext/Form/Module.bsl` — тот же объект), и для
/// sub-config-префиксов (`base/Catalogs/…`, `extensions/X/Catalogs/…`).
/// `None` — модуль вне объектных папок (например, `Configuration/…`).
pub fn object_from_module_path(path: &str) -> Option<(&'static str, String)> {
    let comps: Vec<&str> = path.split(['/', '\\']).collect();
    for i in 0..comps.len().saturating_sub(1) {
        for (folder, meta_type) in MODULE_FOLDERS {
            if comps[i] == *folder {
                return Some((meta_type, comps[i + 1].to_string()));
            }
        }
    }
    None
}

/// Разобранный комментарий над процедурой: проза отдельно, метатеги отдельно.
///
/// Метатеги в прозу не попадают: имя тега и служебные слова (`@security
/// читает адрес…`) в термах дают шум, а `@tags` нужны отдельной фразой —
/// чтобы триграммы матчили слово целиком («весы»), а не кусок фразы
/// («…службе весов из…»).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LeadingComment {
    /// Содержательные строки описания (без `@`-строк), склеенные пробелом
    /// и обрезанные до [`PROSE_LIMIT_CHARS`].
    pub prose: String,
    /// Первая содержательная строка описания, до [`HEAD_LIMIT_CHARS`] —
    /// то, что показывается агенту вместо всей строки термов.
    pub head: String,
    /// Значения `@tags` (в исходнике — через запятую), нормализованные
    /// `fold_text`; порядок исходника сохранён.
    pub tags: Vec<String>,
    /// Значения `@depends` (в исходнике — через точку с запятой).
    pub depends: Vec<String>,
    /// Длина ВСЕГО комментария (проза + строки метатегов) до обрезки.
    /// `0` — описания над процедурой нет; по этой колонке считается аудит
    /// «экспортные процедуры без контракта».
    pub len_chars: usize,
}

impl LeadingComment {
    /// Теги одной строкой через запятую — как в исходнике `@tags`.
    /// Значение колонки `procedure_enrichment.tags`.
    pub fn tags_line(&self) -> String {
        self.tags.join(", ")
    }
}

/// Разобранная шапка модуля — комментарий в начале `.bsl` ДО первого
/// объявления (см. [`extract_module_header`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleHeader {
    /// Проза шапки построчно (строки разделены переводом строки), до
    /// [`HEADER_PROSE_LIMIT_CHARS`]. Значение колонки `module_enrichment.header`.
    pub prose: String,
    /// Значения `@tags` шапки — попадают и в термы процедур модуля.
    pub tags: Vec<String>,
    /// Значения `@depends` шапки.
    pub depends: Vec<String>,
}

impl ModuleHeader {
    /// Первая строка прозы, до [`HEAD_LIMIT_CHARS`] — карточка модуля
    /// в выдаче `get_object_profile`.
    pub fn head(&self) -> String {
        head_line(&self.prose)
    }

    /// Теги одной строкой через запятую (колонка `module_enrichment.tags`).
    pub fn tags_line(&self) -> String {
        self.tags.join(", ")
    }

    /// Зависимости одной строкой через «; » (колонка `module_enrichment.depends`).
    pub fn depends_line(&self) -> String {
        self.depends.join("; ")
    }
}

/// Первая строка текста, обрезанная до [`HEAD_LIMIT_CHARS`].
pub fn head_line(text: &str) -> String {
    cut_chars(text.lines().next().unwrap_or("").trim(), HEAD_LIMIT_CHARS)
}

/// Разобрать строку комментария как метатег `@<имя> <значение>`
/// (регулярка грамматики `code-docs`: `^@(\w+)\s+(.+)$`).
/// `None` — обычная строка описания.
fn parse_meta_tag(body: &str) -> Option<(String, &str)> {
    let rest = body.strip_prefix('@')?;
    // \w+ — буквы (в том числе кириллица), цифры и подчёркивание.
    let name_end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    let (name, tail) = rest.split_at(name_end);
    // \s+ между именем тега и значением обязателен: «@tags» без значения
    // и «@tags:» метатегами не считаются.
    if !tail.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let value = tail.trim();
    if value.is_empty() {
        return None;
    }
    Some((name.to_lowercase(), value))
}

/// Значения метатега через разделитель: нормализация `fold_text`, обрезка
/// пробелов, пустые отбрасываются, дубли схлопываются.
fn split_values(value: &str, sep: char) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in value.split(sep) {
        let v = fold_text(part.trim()).trim().to_string();
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Накопитель разбора блока `//`-строк: проза отдельно, значения `@tags`
/// и `@depends` отдельно. Один разбор и на описание процедуры, и на шапку
/// модуля — грамматика метатегов у них общая.
#[derive(Default)]
struct MetaBlock {
    lines: Vec<String>,
    tags: Vec<String>,
    depends: Vec<String>,
    len_chars: usize,
}

impl MetaBlock {
    /// Добавить содержательную строку комментария (уже без `//` и пробелов).
    fn push(&mut self, body: &str) {
        // Длина комментария считается по всему содержимому, включая строки
        // метатегов: `comment_len` отвечает на вопрос «описание над
        // процедурой есть?», а описание из одних тегов — тоже описание.
        if self.len_chars > 0 {
            self.len_chars += 1; // пробел-склейка между строками
        }
        self.len_chars += body.chars().count();
        match parse_meta_tag(body) {
            Some((name, value)) => match name.as_str() {
                "tags" => {
                    for v in split_values(value, ',') {
                        if !self.tags.contains(&v) {
                            self.tags.push(v);
                        }
                    }
                }
                "depends" => {
                    for v in split_values(value, ';') {
                        if !self.depends.contains(&v) {
                            self.depends.push(v);
                        }
                    }
                }
                // Прочие теги (@layer, @security, @todo, @example) в термах
                // дают больше шума, чем сигнала, — не индексируем их вовсе.
                _ => {}
            },
            None => self.lines.push(body.to_string()),
        }
    }

    fn is_empty(&self) -> bool {
        self.len_chars == 0
    }
}

/// Строка `idx` без BOM (только первая) и без окружающих пробелов.
fn trimmed_line<S: AsRef<str>>(lines: &[S], idx: usize) -> Option<&str> {
    let raw = lines.get(idx)?.as_ref();
    // BOM ядро снимает при чтении, но disk-путь читает файл сам — страхуемся.
    let raw = if idx == 0 { raw.trim_start_matches('\u{feff}') } else { raw };
    Some(raw.trim())
}

/// Комментарий непосредственно над процедурой: идём вверх от строки
/// `line_start` (1-based), пропуская аннотации (`&НаСервере`) и
/// декоративные разделители (`//////`), собираем содержательные `//`-строки.
/// Останавливаемся на первой не-комментарной строке.
///
/// Метатеги (`@tags`, `@depends`, …) отделяются от прозы; проза обрезается
/// до [`PROSE_LIMIT_CHARS`], а `len_chars` считает комментарий целиком —
/// поэтому теги, стоящие по грамматике `code-docs` в конце длинного описания,
/// попадают в термы независимо от предела.
pub fn extract_leading_comment<S: AsRef<str>>(
    lines: &[S],
    line_start: usize,
) -> Option<LeadingComment> {
    if line_start < 2 {
        return None;
    }
    let mut collected: Vec<&str> = Vec::new();
    // line_start 1-based → индекс строки процедуры = line_start-1; выше неё — line_start-2.
    let mut i = line_start - 2;
    loop {
        let t = lines.get(i)?.as_ref().trim();
        if t.starts_with("//") {
            let body = t.trim_start_matches('/').trim();
            // Декоративный разделитель («////////», «//====») — пропустить.
            if !body.is_empty() && !body.chars().all(|c| matches!(c, '=' | '-' | '*' | '/')) {
                collected.push(body);
            }
        } else if t.starts_with('&') {
            // Аннотация компиляции между комментарием и процедурой.
        } else {
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    let mut block = MetaBlock::default();
    for body in collected {
        block.push(body);
    }
    if block.is_empty() {
        return None;
    }
    Some(LeadingComment {
        prose: cut_chars(&block.lines.join(" "), PROSE_LIMIT_CHARS),
        head: cut_chars(
            block.lines.first().map(String::as_str).unwrap_or(""),
            HEAD_LIMIT_CHARS,
        ),
        tags: block.tags,
        depends: block.depends,
        len_chars: block.len_chars,
    })
}

/// Шапка модуля — блок `//`-строк в самом начале `.bsl`, до первого
/// объявления. Идём от начала файла, пропуская пустые строки и декоративные
/// рамки (`////`, `//====`), собираем `//`-строки и останавливаемся на первой
/// не-комментарной.
///
/// Блок считается шапкой, только если за ним идёт пустая строка, `#Область`,
/// `#Если`, `Перем` или конец файла. Если он без пустой строки переходит в
/// `Процедура`/`Функция`/`&Аннотация` — это описание первой процедуры, а не
/// шапка модуля (его возьмёт `extract_leading_comment`), и здесь — `None`.
pub fn extract_module_header<S: AsRef<str>>(lines: &[S]) -> Option<ModuleHeader> {
    let mut i = 0usize;
    while matches!(trimmed_line(lines, i), Some(t) if t.is_empty()) {
        i += 1;
    }
    let mut block = MetaBlock::default();
    while let Some(t) = trimmed_line(lines, i) {
        if !t.starts_with("//") {
            break;
        }
        let body = t.trim_start_matches('/').trim();
        if !body.is_empty() && !body.chars().all(|c| matches!(c, '=' | '-' | '*' | '/')) {
            block.push(body);
        }
        i += 1;
    }
    if block.is_empty() {
        return None;
    }
    // Граница: что стоит СРАЗУ за блоком комментария.
    match trimmed_line(lines, i) {
        // Конец файла — модуль из одного комментария; это шапка.
        None => {}
        Some(t) if t.is_empty() => {}
        Some(t) => {
            let low = fold_text(t);
            let module_level = low.starts_with('#')
                || low.starts_with("перем ")
                || low.starts_with("var ")
                || low == "перем"
                || low == "var";
            if !module_level {
                // Процедура/Функция/&Аннотация сразу за блоком → это описание
                // первой процедуры, а не шапка модуля.
                return None;
            }
        }
    }
    Some(ModuleHeader {
        prose: cut_chars(&block.lines.join("\n"), HEADER_PROSE_LIMIT_CHARS),
        tags: block.tags,
        depends: block.depends,
    })
}

/// Собрать строку термов для одной процедуры. Формат — фразы через запятую
/// (как у LLM-enrich), всё в нижнем регистре. Пустые источники опускаются;
/// дубли фраз схлопываются.
///
/// Порядок: теги процедуры, слова имени, объект, синоним, проза описания,
/// теги шапки модуля. Теги — КАЖДЫЙ отдельной фразой, чтобы триграммный
/// поиск матчил слово целиком («весы»), а не его кусок внутри фразы.
pub fn build_terms(
    proc_name: &str,
    object_name: Option<&str>,
    object_synonym: Option<&str>,
    comment: Option<&LeadingComment>,
    module_tags: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    fn push(p: String, parts: &mut Vec<String>) {
        if !p.is_empty() && !parts.contains(&p) {
            parts.push(p);
        }
    }
    if let Some(c) = comment {
        for tag in &c.tags {
            push(fold_text(tag), &mut parts);
        }
    }
    push(split_identifier(proc_name).join(" "), &mut parts);
    if let Some(obj) = object_name {
        push(split_identifier(obj).join(" "), &mut parts);
    }
    if let Some(syn) = object_synonym {
        push(fold_text(syn), &mut parts);
    }
    if let Some(c) = comment {
        push(fold_text(&c.prose), &mut parts);
    }
    for tag in module_tags {
        push(fold_text(tag), &mut parts);
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_camel_cyrillic() {
        assert_eq!(
            split_identifier("УточнитьДанныеПоШтрихкоду"),
            vec!["уточнить", "данные", "по", "штрихкоду"]
        );
    }

    #[test]
    fn yo_folds_to_e_everywhere() {
        // Идентификаторы: «РасчётСебестоимости» нормализуется без ё.
        assert_eq!(split_identifier("РасчётСебестоимости"), vec!["расчет", "себестоимости"]);
        // Свободный текст (синонимы, комментарии): та же свёртка.
        assert_eq!(fold_text("Учёт партий"), "учет партий");
    }

    #[test]
    fn split_latin_and_underscores() {
        assert_eq!(split_identifier("RefineBarcode"), vec!["refine", "barcode"]);
        assert_eq!(
            split_identifier("ent_ДоработкаОбмена"),
            vec!["ent", "доработка", "обмена"]
        );
    }

    #[test]
    fn split_acronym_and_digits() {
        assert_eq!(split_identifier("XMLReader"), vec!["xml", "reader"]);
        assert_eq!(
            split_identifier("ПолучитьHTTPОтвет"),
            vec!["получить", "http", "ответ"]
        );
        assert_eq!(split_identifier("Форма2Элемент"), vec!["форма", "2", "элемент"]);
    }

    #[test]
    fn object_from_paths() {
        assert_eq!(
            object_from_module_path("Catalogs/Номенклатура/Ext/ObjectModule.bsl"),
            Some(("Catalog", "Номенклатура".to_string()))
        );
        // Форма того же объекта → тот же владелец.
        assert_eq!(
            object_from_module_path(
                "Documents/Реализация/Forms/ФормаДокумента/Ext/Form/Module.bsl"
            ),
            Some(("Document", "Реализация".to_string()))
        );
        // Sub-config-префикс (base/, extensions/<имя>/) не мешает.
        assert_eq!(
            object_from_module_path("base/CommonModules/РаботаСоСкидками/Ext/Module.bsl"),
            Some(("CommonModule", "РаботаСоСкидками".to_string()))
        );
        assert_eq!(object_from_module_path("Configuration/ManagedApplicationModule.bsl"), None);
    }

    #[test]
    fn comment_extraction() {
        let lines = vec![
            "////////////////////////////////",
            "// Уточняет данные по штрихкоду.",
            "// Параметры: Штрихкод - Строка.",
            "&НаСервере",
            "Процедура УточнитьДанные()",
        ];
        // Процедура на строке 5 (1-based).
        let c = extract_leading_comment(&lines, 5).expect("комментарий есть");
        assert_eq!(c.prose, "Уточняет данные по штрихкоду. Параметры: Штрихкод - Строка.");
        assert_eq!(c.head, "Уточняет данные по штрихкоду.");
        assert!(c.tags.is_empty());
        assert_eq!(c.len_chars, c.prose.chars().count());
        // Нет комментария над строкой 1.
        assert!(extract_leading_comment(&lines, 1).is_none());
    }

    #[test]
    fn comment_stops_at_code() {
        let lines = vec![
            "КонецПроцедуры",
            "",
            "Процедура Другая()",
        ];
        // Пустая строка над процедурой → комментария нет.
        assert!(extract_leading_comment(&lines, 3).is_none());
    }

    #[test]
    fn meta_tags_survive_prose_limit() {
        // Образец КА: описание длиннее 240 символов, метатеги — в его конце
        // (грамматика code-docs). Теги обязаны попасть в структуру, хотя
        // проза до них обрезана.
        let long = "Читает настройки подключения к службе весов из безопасного хранилища БСП.";
        let lines = vec![
            format!("// {}", long),
            format!("// {}", long),
            format!("// {}", long),
            format!("// {}", long),
            "//".to_string(),
            "// @tags Весы, Настройки".to_string(),
            "// @depends РегистрСведений.БезопасноеХранилищеДанных; ОбщегоНазначения".to_string(),
            "// @security читает адрес/порт локальной службы".to_string(),
            "Функция ПрочитатьНастройки() Экспорт".to_string(),
        ];
        let c = extract_leading_comment(&lines, 9).expect("комментарий есть");
        assert_eq!(c.tags, vec!["весы", "настройки"]);
        assert_eq!(
            c.depends,
            vec!["регистрсведений.безопасноехранилищеданных", "общегоназначения"]
        );
        // Проза обрезана пределом, а `@`-строк в ней нет ни одной.
        assert!(c.prose.chars().count() <= PROSE_LIMIT_CHARS);
        assert!(!c.prose.contains('@'));
        assert!(!c.prose.contains("security"));
        // Длина считается по всему комментарию, включая строки тегов.
        assert!(c.len_chars > PROSE_LIMIT_CHARS);
        assert_eq!(c.head, long);
        // @security термов не создаёт, теги идут первыми.
        let terms = build_terms("ПрочитатьНастройки", None, None, Some(&c), &[]);
        assert!(terms.starts_with("весы, настройки, прочитать настройки"));
        assert!(!terms.contains("security"));
        assert!(!terms.contains("адрес/порт"));
    }

    #[test]
    fn comment_of_tags_only_has_empty_prose() {
        let lines = vec!["// @tags Весы", "Процедура П()"];
        let c = extract_leading_comment(&lines, 2).expect("комментарий есть");
        assert!(c.prose.is_empty());
        assert!(c.head.is_empty());
        assert_eq!(c.tags, vec!["весы"]);
        assert!(c.len_chars > 0, "описание из одних тегов — тоже описание");
    }

    #[test]
    fn tags_fold_case_and_yo() {
        let lines = vec!["// @tags Расчёт Себестоимости, ВЕСЫ", "Процедура П()"];
        let c = extract_leading_comment(&lines, 2).expect("комментарий есть");
        assert_eq!(c.tags, vec!["расчет себестоимости", "весы"]);
        assert_eq!(c.tags_line(), "расчет себестоимости, весы");
    }

    #[test]
    fn module_header_of_ka_sample() {
        // Образец КА: рамка, строка «Общий модуль…», абзац, @layer, @tags.
        let lines = vec![
            "\u{feff}////////////////////////////////////////////////////////",
            "// Общий модуль «КРБ_ИнтеграцияВесов» (флаг: Сервер).",
            "//",
            "// Читает настройки подключения к службе весов из хранилища БСП.",
            "//",
            "// @layer infra",
            "// @tags весы, служба, настройки, интеграция",
            "////////////////////////////////////////////////////////",
            "",
            "#Область ПрограммныйИнтерфейс",
        ];
        let h = extract_module_header(&lines).expect("шапка есть");
        assert_eq!(h.tags, vec!["весы", "служба", "настройки", "интеграция"]);
        assert_eq!(h.head(), "Общий модуль «КРБ_ИнтеграцияВесов» (флаг: Сервер).");
        assert!(h.prose.contains("Читает настройки"));
        // @layer в тегах не участвует и в прозе не остаётся.
        assert!(!h.prose.contains("@layer"));
        assert!(!h.prose.contains("infra"));
    }

    #[test]
    fn module_header_absent_cases() {
        // Файл начинается с #Область — шапки нет.
        assert!(extract_module_header(&["#Область ПрограммныйИнтерфейс", "Процедура П()"]).is_none());
        // Комментарий первой процедуры без шапки: блок сразу переходит
        // в объявление, пустой строки между ними нет.
        assert!(extract_module_header(&["// Делает что-то.", "Процедура П() Экспорт"]).is_none());
        // То же с аннотацией компиляции между комментарием и процедурой.
        assert!(extract_module_header(&["// Делает что-то.", "&НаСервере", "Процедура П()"]).is_none());
        // Пустой модуль (только BOM) — шапки нет.
        assert!(extract_module_header(&["\u{feff}"]).is_none());
        let empty: [&str; 0] = [];
        assert!(extract_module_header(&empty).is_none());
    }

    #[test]
    fn module_header_before_blank_line_and_procedure() {
        let lines = vec![
            "// Модуль обмена с весами.",
            "// @tags весы",
            "",
            "Процедура Обменять() Экспорт",
        ];
        let h = extract_module_header(&lines).expect("шапка есть");
        assert_eq!(h.prose, "Модуль обмена с весами.");
        assert_eq!(h.tags, vec!["весы"]);
        // Модуль из одного комментария (конец файла сразу за блоком).
        let h2 = extract_module_header(&["// Только шапка."]).expect("шапка есть");
        assert_eq!(h2.prose, "Только шапка.");
        // Перем на модульном уровне — тоже граница шапки.
        let h3 = extract_module_header(&["// Шапка.", "Перем Кэш;"]).expect("шапка есть");
        assert_eq!(h3.prose, "Шапка.");
    }

    #[test]
    fn build_terms_full_and_dedup() {
        let lines = vec!["// Уточняет штрихкод товара", "Процедура П()"];
        let c = extract_leading_comment(&lines, 2);
        let t = build_terms(
            "УточнитьШтрихкод",
            Some("Номенклатура"),
            Some("Номенклатура"),
            c.as_ref(),
            &[],
        );
        // Синоним «Номенклатура» дублирует слова объекта → схлопнут.
        assert_eq!(t, "уточнить штрихкод, номенклатура, уточняет штрихкод товара");

        let t2 = build_terms("RefineBarcode", None, None, None, &[]);
        assert_eq!(t2, "refine barcode");
    }

    #[test]
    fn build_terms_order_with_module_tags() {
        let lines = vec!["// @tags весы", "// Читает настройки.", "Процедура П()"];
        let c = extract_leading_comment(&lines, 3).expect("комментарий есть");
        let module_tags = vec!["весы".to_string(), "служба".to_string()];
        let t = build_terms(
            "ПрочитатьНастройки",
            Some("КРБ_ИнтеграцияВесов"),
            None,
            Some(&c),
            &module_tags,
        );
        // Порядок: теги процедуры, имя, объект, синоним, проза, теги модуля;
        // «весы» уже был тегом процедуры → из тегов модуля не дублируется.
        assert_eq!(
            t,
            "весы, прочитать настройки, крб интеграция весов, читает настройки., служба"
        );
    }
}
