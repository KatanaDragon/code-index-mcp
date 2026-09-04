// Парсер XML-описаний управляемых форм 1С (`*/Forms/<Имя>/Form.xml`
// или `*/Forms/<Имя>/Ext/Form.xml` в зависимости от выгрузки).
//
// Назначение — извлечь обработчики событий формы. На уровне XML
// это выглядит так:
//
//   <Form>
//     <Events>
//       <Event name="ПриОткрытии">ПриОткрытии</Event>
//       <Event name="ПередЗакрытием">ПередЗакрытиемОбработчик</Event>
//     </Events>
//   </Form>
//
// `name` — имя события платформы 1С, текстовое содержимое тега —
// имя процедуры в модуле формы. Они часто совпадают, но БСП-расширения
// могут проксировать стандартные события на свои обработчики, тогда
// имена расходятся.
//
// Реальные дампы 1С могут отличаться по namespace и обёрткам; парсер
// делает мягкое сопоставление по local-имени тега, без жёсткой
// привязки к конкретной структуре XML — это позволяет обрабатывать
// и DumpConfigToFiles, и v8unpack-выгрузку, и форматы расширений.

use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Один обработчик события формы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormHandler {
    /// Имя события платформы 1С: `ПриОткрытии`, `ПередЗакрытием`, `ПриСозданииНаСервере`...
    pub event: String,
    /// Имя процедуры в модуле формы, на которую назначен обработчик.
    pub handler: String,
    /// Элемент формы, которому принадлежит обработчик (`СуммаДокумента`,
    /// `ТоварыКоличество`). `None` — обработчик самой формы, а не элемента.
    /// Имена элементов уникальны в пределах формы, поэтому путь вложенности
    /// не хранится: к элементу обращаются как `Элементы.<Имя>`.
    pub element: Option<String>,
}

/// Нормализует имя события ФОРМЫ к русскому виду (как в конфигураторе).
///
/// В выгрузке — и в формате Конфигуратора, и в EDT — события формы и её
/// элементов записаны английскими идентификаторами платформы (`OnChange`,
/// `OnCreateAtServer`); проверено на пяти базах: русских имён нет ни одного.
/// Разработчик же видит в конфигураторе русские названия, и таблица подписок
/// (`event_subscriptions`) тоже хранит русские — поэтому переводим и здесь,
/// чтобы выдача была единообразной.
///
/// Словарь событий ПОДПИСОК (`event_subscriptions::event_to_russian`) для этого
/// не годится: он собран под события объектов и с событиями форм пересекается
/// всего в нескольких именах — от него получалась мешанина, где `ПередЗаписью`
/// стоит рядом с непереведённым `BeforeWriteAtServer`.
///
/// Неизвестные имена возвращаются как есть — ничего не теряется. Так же
/// проходят идентификаторы событий внешних компонент (`9cc34712-da5f-…`),
/// которым русского имени не существует.
///
/// Соответствия не выдуманы: каждое сверено по самим конфигурациям. Обработчики
/// в 1С называют по образцу «ИмяЭлемента + ИмяСобытия»
/// (`СуммаДокументаПриИзменении`), поэтому русское имя события читается из
/// хвоста имени процедуры. Проверка «сколько обработчиков события кончаются на
/// предполагаемое имя» прогнана по четырём типовым конфигурациям; она же
/// поправила два неверных перевода (`EditTextChange` — без приставки «При»,
/// `AdditionalDetailProcessing` — другой порядок слов).
pub fn form_event_to_russian(event: &str) -> &str {
    match event {
        // ── События самой формы ──────────────────────────────────────────
        "OnOpen" => "ПриОткрытии",
        "OnClose" => "ПриЗакрытии",
        "BeforeClose" => "ПередЗакрытием",
        "OnCreateAtServer" => "ПриСозданииНаСервере",
        "OnReadAtServer" => "ПриЧтенииНаСервере",
        "BeforeWrite" => "ПередЗаписью",
        "BeforeWriteAtServer" => "ПередЗаписьюНаСервере",
        "OnWriteAtServer" => "ПриЗаписиНаСервере",
        "AfterWrite" => "ПослеЗаписи",
        "AfterWriteAtServer" => "ПослеЗаписиНаСервере",
        "FillCheckProcessingAtServer" => "ОбработкаПроверкиЗаполненияНаСервере",
        "NotificationProcessing" => "ОбработкаОповещения",
        "ChoiceProcessing" => "ОбработкаВыбора",
        "NewWriteProcessing" => "ОбработкаЗаписиНового",
        "OnReopen" => "ПриПовторномОткрытии",
        "ExternalEvent" => "ВнешнееСобытие",
        "URLProcessing" => "ОбработкаНавигационнойСсылки",
        "OnSaveDataInSettingsAtServer" => "ПриСохраненииДанныхВНастройкахНаСервере",
        "OnLoadDataFromSettingsAtServer" => "ПриЗагрузкеДанныхИзНастроекНаСервере",
        "BeforeLoadDataFromSettingsAtServer" => "ПередЗагрузкойДанныхИзНастроекНаСервере",
        // ── События элементов формы ──────────────────────────────────────
        "OnChange" => "ПриИзменении",
        "StartChoice" => "НачалоВыбора",
        "StartListChoice" => "НачалоВыбораИзСписка",
        "Clearing" => "Очистка",
        "Opening" => "Открытие",
        "AutoComplete" => "АвтоПодбор",
        "TextEditEnd" => "ОкончаниеВводаТекста",
        "EditTextChange" => "ИзменениеТекстаРедактирования",
        "Click" => "Нажатие",
        "Selection" => "Выбор",
        "ValueChoice" => "ВыборЗначения",
        "BeforeAddRow" => "ПередНачаломДобавления",
        "BeforeRowChange" => "ПередНачаломИзменения",
        "BeforeDeleteRow" => "ПередУдалением",
        "AfterDeleteRow" => "ПослеУдаления",
        "OnStartEdit" => "ПриНачалеРедактирования",
        "OnEditEnd" => "ПриОкончанииРедактирования",
        "BeforeEditEnd" => "ПередОкончаниемРедактирования",
        "OnActivateRow" => "ПриАктивизацииСтроки",
        "OnActivateCell" => "ПриАктивизацииЯчейки",
        "OnActivateField" => "ПриАктивизацииПоля",
        "OnActivate" => "ПриАктивизации",
        "Drag" => "Перетаскивание",
        "DragStart" => "НачалоПеретаскивания",
        "DragEnd" => "ОкончаниеПеретаскивания",
        "DragCheck" => "ПроверкаПеретаскивания",
        "BeforeExpand" => "ПередРазворачиванием",
        "BeforeCollapse" => "ПередСворачиванием",
        "OnCurrentPageChange" => "ПриСменеСтраницы",
        "OnGetDataAtServer" => "ПриПолученииДанныхНаСервере",
        "DetailProcessing" => "ОбработкаРасшифровки",
        "AdditionalDetailProcessing" => "ОбработкаДополнительнойРасшифровки",
        "DocumentComplete" => "ДокументСформирован",
        "OnClick" => "ПриНажатии",
        "Creating" => "Создание",
        // ── Расширение формы отчёта ──────────────────────────────────────
        "OnUpdateUserSettingSetAtServer" => "ПриОбновленииСоставаПользовательскихНастроекНаСервере",
        "OnSaveUserSettingsAtServer" => "ПриСохраненииПользовательскихНастроекНаСервере",
        "OnLoadUserSettingsAtServer" => "ПриЗагрузкеПользовательскихНастроекНаСервере",
        "BeforeLoadUserSettingsAtServer" => "ПередЗагрузкойПользовательскихНастроекНаСервере",
        "OnSaveVariantAtServer" => "ПриСохраненииВариантаНаСервере",
        "OnLoadVariantAtServer" => "ПриЗагрузкеВариантаНаСервере",
        "BeforeLoadVariantAtServer" => "ПередЗагрузкойВариантаНаСервере",
        "BeforePrint" => "ПередПечатью",
        // ── Редкие события: таблица, календарь, дерево, планировщик,
        //    поле HTML-документа, система взаимодействия ────────────────────
        "Tuning" => "Регулирование",
        "RefreshRequestProcessing" => "ОбработкаЗапросаОбновления",
        "URLGetProcessing" => "ОбработкаПолученияНавигационнойСсылки",
        "URLListGetProcessing" => "ОбработкаПолученияСпискаНавигационныхСсылок",
        "NavigationProcessing" => "ОбработкаПерехода",
        "ActivationProcessing" => "ОбработкаАктивизации",
        "OnChangeAreaContent" => "ПриИзмененииСодержимогоОбласти",
        "OnPeriodOutput" => "ПриВыводеПериода",
        "OnActivateDate" => "ПриАктивизацииДаты",
        "OnCurrentParentChange" => "ПриСменеТекущегоРодителя",
        "OnCurrentRepresentationPeriodChange" => "ПриСменеТекущегоПериодаОтображения",
        "MultipleValueOpening" => "ОткрытиеМножественногоЗначения",
        "MultipleValuesDelete" => "УдалениеМножественныхЗначений",
        "BeforeStartEdit" => "ПередНачаломРедактирования",
        "BeforeStartQuickEdit" => "ПередНачаломБыстрогоРедактирования",
        "BeforeCreate" => "ПередСозданием",
        "BeforeDelete" => "ПередУдалением",
        "BeforeExecute" => "ПередВыполнением",
        "CommandGenerateProcessing" => "ОбработкаФормированияКоманд",
        "OnMainServerAvailabilityChange" => "ПриИзмененииДоступностиОсновногоСервера",
        "OnClientApplicationSuspend" => "ПриЗасыпанииКлиентскогоПриложения",
        "OnClientApplicationResume" => "ПриПробужденииКлиентскогоПриложения",
        "OnReopenFromOtherServer" => "ПриПереоткрытииСДругогоСервера",
        "BeforeReopenFromOtherServer" => "ПередПереоткрытиемСДругогоСервера",
        "AddInDetachmentOnError" => "ОтключениеВнешнейКомпонентыПриОшибке",
        "OnChangeDisplaySettings" => "ПриИзмененииПараметровЭкрана",
        "CollaborationSystemUsersAutoComplete" => "АвтоПодборПользователейСистемыВзаимодействия",
        "CollaborationSystemUsersChoiceFormGetProcessing" => {
            "ОбработкаПолученияФормыВыбораПользователейСистемыВзаимодействия"
        }
        other => other,
    }
}

/// Сериализовать обработчики для колонки `metadata_forms.handlers_json`.
/// Формат общий для обоих форматов выгрузки; `element` опускается у
/// обработчиков самой формы.
pub fn handlers_to_json(handlers: &[FormHandler]) -> Result<String> {
    let arr: Vec<serde_json::Value> = handlers
        .iter()
        .map(|h| match &h.element {
            Some(el) => serde_json::json!({
                "event": h.event,
                "handler": h.handler,
                "element": el,
            }),
            None => serde_json::json!({"event": h.event, "handler": h.handler}),
        })
        .collect();
    Ok(serde_json::to_string(&arr)?)
}

/// Распарсить XML-описание формы.
pub fn parse_form_xml(content: &str) -> Result<Vec<FormHandler>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut current_event_name: Option<String> = None;
    let mut tag_stack: Vec<String> = Vec::new();
    // Владельцы обработчиков: по элементу формы на каждый открытый тег с
    // атрибутом `name` (`<InputField name="Товар">`, `<Table name="Товары">`).
    // Владелец обработчика — ближайший такой предок; пусто — сама форма.
    let mut owner_stack: Vec<Option<String>> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                // Атрибут `name` есть и у тега события (там это имя события),
                // и у тегов элементов формы (там это имя элемента).
                let mut name_value: Option<String> = None;
                for attr in e.attributes().with_checks(false) {
                    if let Ok(a) = attr {
                        if local_name(a.key.as_ref()) == "name" {
                            let v = a
                                .unescape_value()
                                .map(|s| s.into_owned())
                                .unwrap_or_default();
                            name_value = Some(v);
                        }
                    }
                }
                if local == "Event" {
                    current_event_name = name_value;
                    owner_stack.push(None);
                } else {
                    owner_stack.push(name_value.filter(|v| !v.is_empty()));
                }
                tag_stack.push(local);
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "Event" {
                    current_event_name = None;
                }
                tag_stack.pop();
                owner_stack.pop();
            }
            Ok(Event::Text(text)) => {
                let parent = tag_stack.last().map(|s| s.as_str()).unwrap_or("");
                if parent == "Event" {
                    if let Some(event_name) = &current_event_name {
                        let handler_name = text
                            .unescape()
                            .map(|s| s.into_owned())
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if !handler_name.is_empty() && !event_name.is_empty() {
                            // Ближайший предок с именем — сам тег `Event`
                            // владельцем не считается (он уже вытолкнут в None).
                            let element = owner_stack.iter().rev().find_map(|o| o.clone());
                            out.push(FormHandler {
                                event: form_event_to_russian(event_name).to_string(),
                                handler: handler_name,
                                element,
                            });
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Form XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ));
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Прочитать XML формы по пути. Возвращает пустой Vec если файла нет —
/// форма может быть закодирована в другом формате выгрузки.
pub fn parse_form_file(path: &Path) -> Result<Vec<FormHandler>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    parse_form_xml(&content)
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name).into_owned();
    match s.find(':') {
        Some(idx) => s[idx + 1..].to_string(),
        None => s,
    }
}

// ── Ссылки формы на объекты конфигурации ─────────────────────────────────
//
// Описание формы ссылается на объекты не только обработчиками. Реквизит формы
// типа `СправочникСсылка.Контрагенты`, параметр формы, динамический список с
// основной таблицей `Документ.Заказ` и его ручной запрос — всё это связи,
// которых нет ни в XML объекта-владельца, ни в модуле. Форма подбора одного
// документа, показывающая список другого, до этого в карте влияния второго
// не значилась: переименование ломало форму молча.
//
// Разбор мягкий, по локальным именам тегов, как у обработчиков выше:
//
//   <Attributes>
//     <Attribute name="Контрагент"><Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
//     <Attribute name="Список"><Type><v8:Type>cfg:DynamicList</v8:Type></Type>
//       <Settings xsi:type="DynamicList">
//         <QueryText>ВЫБРАТЬ … ИЗ Документ.Заказ КАК З</QueryText>
//         <MainTable>Document.Заказ</MainTable>
//     <Attribute name="Таблица"><Columns><Column name="Склад"><Type>…
//   <Parameters>
//     <Parameter name="Склад"><Type><v8:Type>cfg:CatalogRef.Склады</v8:Type></Type>

use super::object_attributes::{edges_from_types, DataLinkEdge};

/// Ссылки описания формы на объекты конфигурации.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FormRefs {
    /// Рёбра от владельца формы к объектам. `from_path` — имя реквизита или
    /// параметра формы (у колонки реквизита-таблицы — `Реквизит.Колонка`),
    /// без имени формы: префикс `Form.<Имя>.` добавляет вызывающий, который
    /// знает имя формы по пути файла.
    pub edges: Vec<DataLinkEdge>,
    /// Тексты запросов динамических списков с ручным запросом: номер строки
    /// файла, на которой открылся тег `<QueryText>` (1-based), и текст как в
    /// файле — вместе с ведущими переводами строк, чтобы номера строк внутри
    /// текста считались от той же точки.
    pub queries: Vec<(usize, String)>,
}

/// Виды рёбер `data_links`, порождаемых формами. Полный пересбор и
/// пофайловое обновление сносят строки только этих видов.
pub const FORM_LINK_KINDS: &[&str] = &["form_attr", "form_param", "form_main_table"];

/// Тип реквизита формы → тип, понятный `classify_type`.
///
/// У реквизитов объектов в XML стоят только ссылки (`cfg:CatalogRef.X`), у
/// реквизитов форм — ещё и объектные типы: основной реквизит формы документа
/// имеет тип `cfg:DocumentObject.X`, форма набора записей —
/// `cfg:InformationRegisterRecordSet.X`. Для графа данных это та же связь с
/// объектом, поэтому суффикс приводится к `Ref`. Типы без имени объекта
/// (`cfg:DynamicList`, `cfg:ValueTable`) не трогаем — `classify_type` их
/// отбросит сам.
fn form_type_to_ref(t: &str) -> String {
    // Длинные суффиксы раньше коротких: `RecordSet` не должен пройти как `Record`.
    const SUFFIXES: &[&str] = &[
        "RecordSet", "RecordManager", "RecordKey", "Record", "Object", "Manager", "List", "Selection",
    ];
    let trimmed = t.trim();
    if let Some(rest) = trimmed.strip_prefix("cfg:") {
        if let Some((head, name)) = rest.split_once('.') {
            for s in SUFFIXES {
                if let Some(kind) = head.strip_suffix(s) {
                    if !kind.is_empty() {
                        return format!("cfg:{}Ref.{}", kind, name);
                    }
                }
            }
        }
    }
    trimmed.to_string()
}

/// Накопитель типов одного реквизита/параметра/колонки формы.
struct FieldAcc {
    from_path: String,
    kind: &'static str,
    types: Vec<String>,
}

/// Номер строки (1-based), на которой лежит байтовая позиция `pos`.
fn line_at(content: &str, pos: usize) -> usize {
    let end = pos.min(content.len());
    content.as_bytes()[..end].iter().filter(|&&b| b == b'\n').count() + 1
}

/// Значение атрибута `name` открывающего тега, если есть.
fn attr_name(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    for attr in e.attributes().with_checks(false).flatten() {
        if local_name(attr.key.as_ref()) == "name" {
            return Some(attr.unescape_value().map(|s| s.into_owned()).unwrap_or_default());
        }
    }
    None
}

/// Открытый тег: локальное имя, атрибут `name`, заведён ли на него накопитель.
type OpenTag = (String, Option<String>, bool);

fn parent_is(stack: &[OpenTag], local: &str) -> bool {
    stack.last().map(|(l, _, _)| l == local).unwrap_or(false)
}

/// Имя ближайшего открытого тега `local` с непустым атрибутом `name`.
fn nearest_named(stack: &[OpenTag], local: &str) -> Option<String> {
    stack
        .iter()
        .rev()
        .find(|(l, n, _)| l == local && n.as_deref().map(|s| !s.is_empty()).unwrap_or(false))
        .and_then(|(_, n, _)| n.clone())
}

fn inside(stack: &[OpenTag], local: &str) -> bool {
    stack.iter().any(|(l, _, _)| l == local)
}

/// Текст лежит прямо в `<Type>`/`<v8:Type>` накопителя: между ним и текстом —
/// только теги `Type`. Иначе это чужой `<Type>` (например, поле внутри
/// настроек динамического списка), и в тип реквизита он не идёт.
fn type_belongs_to_field(stack: &[OpenTag]) -> bool {
    let mut saw_type = false;
    for (l, _, has_acc) in stack.iter().rev() {
        if l == "Type" {
            saw_type = true;
            continue;
        }
        return saw_type && *has_acc;
    }
    false
}

/// Распарсить XML формы: рёбра к объектам и тексты запросов динамических списков.
pub fn parse_form_refs_xml(content: &str) -> Result<FormRefs> {
    let mut reader = Reader::from_str(content);
    // Без обрезки пробелов: текст запроса берём как в файле, чтобы номера строк
    // внутри него отсчитывались от строки открывающего тега.
    reader.config_mut().trim_text(false);

    let mut out = FormRefs::default();
    let mut buf = Vec::new();
    let mut stack: Vec<OpenTag> = Vec::new();
    let mut fields: Vec<FieldAcc> = Vec::new();
    // Строка открывающего тега текущего `<QueryText>` (None — не внутри него).
    let mut query_line: Option<usize> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                let name_attr = attr_name(&e);
                let mut has_acc = false;
                match local.as_str() {
                    "Attribute" | "Parameter" | "Column" => {
                        let named = name_attr.as_deref().filter(|n| !n.is_empty());
                        // Реквизит и параметр — только на своём уровне, колонка —
                        // внутри реквизита-таблицы. Вложенные одноимённые теги
                        // в настройках списков накопителя не заводят.
                        let acc = match (local.as_str(), named) {
                            ("Attribute", Some(n)) if parent_is(&stack, "Attributes") => {
                                Some((n.to_string(), "form_attr"))
                            }
                            ("Parameter", Some(n)) if parent_is(&stack, "Parameters") => {
                                Some((n.to_string(), "form_param"))
                            }
                            ("Column", Some(n)) if parent_is(&stack, "Columns") => {
                                nearest_named(&stack, "Attribute")
                                    .map(|a| (format!("{}.{}", a, n), "form_attr"))
                            }
                            _ => None,
                        };
                        if let Some((from_path, kind)) = acc {
                            fields.push(FieldAcc { from_path, kind, types: Vec::new() });
                            has_acc = true;
                        }
                    }
                    "QueryText" if inside(&stack, "Settings") => {
                        query_line = Some(line_at(content, reader.buffer_position() as usize));
                    }
                    _ => {}
                }
                stack.push((local, name_attr, has_acc));
            }
            Ok(Event::End(_)) => {
                if let Some((local, _, has_acc)) = stack.pop() {
                    if has_acc {
                        if let Some(f) = fields.pop() {
                            out.edges.extend(edges_from_types(f.from_path, &f.types, f.kind));
                        }
                    }
                    if local == "QueryText" {
                        query_line = None;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                collect_text(&stack, &mut fields, &mut out, query_line, &text);
            }
            Ok(Event::CData(c)) => {
                let text = String::from_utf8_lossy(c.into_inner().as_ref()).into_owned();
                collect_text(&stack, &mut fields, &mut out, query_line, &text);
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Form XML: ошибка парсинга ссылок на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ));
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Разложить текстовый узел по назначению: тип реквизита, основная таблица
/// динамического списка либо кусок текста его запроса.
fn collect_text(
    stack: &[OpenTag],
    fields: &mut [FieldAcc],
    out: &mut FormRefs,
    query_line: Option<usize>,
    text: &str,
) {
    let top = match stack.last() {
        Some((l, _, _)) => l.as_str(),
        None => return,
    };
    match top {
        "Type" => {
            if type_belongs_to_field(stack) {
                if let Some(f) = fields.last_mut() {
                    let t = text.trim();
                    if !t.is_empty() {
                        f.types.push(form_type_to_ref(t));
                    }
                }
            }
        }
        "MainTable" if inside(stack, "Settings") => {
            let to_object = crate::code_usages::normalize_object_ref(text.trim()).into_owned();
            if let Some(attr) = nearest_named(stack, "Attribute") {
                if to_object.contains('.') {
                    out.edges.push(DataLinkEdge {
                        from_path: attr,
                        to_object,
                        link_kind: "form_main_table",
                        is_composite: false,
                        is_universal: false,
                    });
                }
            }
        }
        "QueryText" => {
            if let Some(line) = query_line {
                // Текст может прийти несколькими узлами (сущности, CDATA) —
                // склеиваем в один запрос той же строки.
                match out.queries.last_mut() {
                    Some((l, q)) if *l == line => q.push_str(text),
                    _ => out.queries.push((line, text.to_string())),
                }
            }
        }
        _ => {}
    }
}

/// Прочитать ссылки формы по пути. Файла нет — пустой результат.
pub fn parse_form_refs_file(path: &Path) -> Result<FormRefs> {
    if !path.is_file() {
        return Ok(FormRefs::default());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    parse_form_refs_xml(&content)
}

#[cfg(test)]
mod refs_tests {
    use super::*;

    const FORM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Attributes>
    <Attribute name="Объект" id="1">
      <Type><v8:Type>cfg:DocumentObject.Заказ</v8:Type></Type>
      <MainAttribute>true</MainAttribute>
    </Attribute>
    <Attribute name="Контрагент" id="2">
      <Type>
        <v8:Type>cfg:CatalogRef.Контрагенты</v8:Type>
        <v8:Type>cfg:CatalogRef.Организации</v8:Type>
      </Type>
    </Attribute>
    <Attribute name="Список" id="3">
      <Type><v8:Type>cfg:DynamicList</v8:Type></Type>
      <Settings xsi:type="DynamicList">
        <ManualQuery>true</ManualQuery>
        <QueryText>ВЫБРАТЬ
	Р.Ссылка КАК Ссылка
ИЗ
	Документ.Реализация КАК Р
	ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Склады КАК С
	ПО Истина</QueryText>
        <Fields><Field><Type><v8:Type>cfg:CatalogRef.Пользователи</v8:Type></Type></Field></Fields>
        <MainTable>Document.Реализация</MainTable>
      </Settings>
    </Attribute>
    <Attribute name="Таблица" id="4">
      <Type><v8:Type>cfg:ValueTable</v8:Type></Type>
      <Columns>
        <Column name="Склад" id="5">
          <Type><v8:Type>cfg:CatalogRef.Склады</v8:Type></Type>
        </Column>
        <Column name="Сумма" id="6">
          <Type><v8:Type>xs:decimal</v8:Type></Type>
        </Column>
      </Columns>
    </Attribute>
  </Attributes>
  <Parameters>
    <Parameter name="Склад">
      <Type><v8:Type>cfg:CatalogRef.Склады</v8:Type></Type>
    </Parameter>
    <Parameter name="Флаг">
      <Type><v8:Type>xs:boolean</v8:Type></Type>
    </Parameter>
  </Parameters>
</Form>
"#;

    fn edge_tuples(r: &FormRefs) -> Vec<(String, String, &'static str, bool)> {
        r.edges
            .iter()
            .map(|e| (e.from_path.clone(), e.to_object.clone(), e.link_kind, e.is_composite))
            .collect()
    }

    #[test]
    fn form_attribute_types_become_edges() {
        let r = parse_form_refs_xml(FORM).unwrap();
        let edges = edge_tuples(&r);
        // Объектный тип основного реквизита приведён к ссылке.
        assert!(edges.contains(&("Объект".into(), "Document.Заказ".into(), "form_attr", false)), "{edges:?}");
        // Составной тип — два ребра с признаком составного.
        assert!(edges.contains(&("Контрагент".into(), "Catalog.Контрагенты".into(), "form_attr", true)));
        assert!(edges.contains(&("Контрагент".into(), "Catalog.Организации".into(), "form_attr", true)));
        // Колонка таблицы — путь `Реквизит.Колонка`; примитивная колонка ребра не даёт.
        assert!(edges.contains(&("Таблица.Склад".into(), "Catalog.Склады".into(), "form_attr", false)));
        assert!(!edges.iter().any(|(p, _, _, _)| p == "Таблица.Сумма"));
        // Параметр формы.
        assert!(edges.contains(&("Склад".into(), "Catalog.Склады".into(), "form_param", false)));
        assert!(!edges.iter().any(|(p, _, _, _)| p == "Флаг"));
    }

    #[test]
    fn dynamic_list_main_table_and_query() {
        let r = parse_form_refs_xml(FORM).unwrap();
        let edges = edge_tuples(&r);
        assert!(edges.contains(&("Список".into(), "Document.Реализация".into(), "form_main_table", false)), "{edges:?}");
        // `<Type>` поля внутри настроек списка типом реквизита не считается.
        assert!(!edges.iter().any(|(_, t, _, _)| t == "Catalog.Пользователи"));
        // Сам DynamicList — не ссылка на объект.
        assert!(!edges.iter().any(|(_, t, _, _)| t.contains("DynamicList")));

        assert_eq!(r.queries.len(), 1);
        let (line, text) = &r.queries[0];
        assert_eq!(*line, 18, "строка открывающего тега <QueryText>");
        assert!(text.starts_with("ВЫБРАТЬ"));
        assert!(text.contains("Справочник.Склады"));
        // Обращения из текста: путь на 4-й строке текста → 21-я строка файла.
        let usages = crate::code_usages::extract_query_usages(text, *line, "form_query");
        let real = usages.iter().find(|u| u.object_ref == "Document.Реализация").unwrap();
        assert_eq!(real.line, 21);
    }

    #[test]
    fn form_type_suffixes_normalized() {
        assert_eq!(form_type_to_ref("cfg:DocumentObject.Заказ"), "cfg:DocumentRef.Заказ");
        assert_eq!(
            form_type_to_ref("cfg:InformationRegisterRecordSet.Курсы"),
            "cfg:InformationRegisterRef.Курсы"
        );
        assert_eq!(form_type_to_ref("cfg:CatalogRef.Склады"), "cfg:CatalogRef.Склады");
        assert_eq!(form_type_to_ref("cfg:DynamicList"), "cfg:DynamicList");
        assert_eq!(form_type_to_ref("xs:string"), "xs:string");
    }

    #[test]
    fn form_without_refs_is_empty() {
        let r = parse_form_refs_xml(
            "<Form><Events><Event name=\"OnOpen\">ПриОткрытии</Event></Events></Form>",
        )
        .unwrap();
        assert!(r.edges.is_empty());
        assert!(r.queries.is_empty());
        assert!(parse_form_refs_file(std::path::Path::new("/non/existent.xml"))
            .unwrap()
            .edges
            .is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form>
  <Properties>
    <Title><v8:item lang="ru"><v8:content>Форма документа</v8:content></v8:item></Title>
  </Properties>
  <Events>
    <Event name="ПриОткрытии">ПриОткрытии</Event>
    <Event name="ПередЗакрытием">ПередЗакрытиемОбработчик</Event>
    <Event name="ПриСозданииНаСервере">ПриСозданииНаСервере</Event>
  </Events>
</Form>
"#;

    #[test]
    fn parses_three_handlers() {
        let handlers = parse_form_xml(SAMPLE).unwrap();
        assert_eq!(handlers.len(), 3);
    }

    #[test]
    fn handler_with_renamed_proc() {
        let handlers = parse_form_xml(SAMPLE).unwrap();
        let renamed = handlers
            .iter()
            .find(|h| h.event == "ПередЗакрытием")
            .unwrap();
        assert_eq!(renamed.handler, "ПередЗакрытиемОбработчик");
    }

    #[test]
    fn ignores_text_outside_event_tag() {
        // <v8:content>Форма документа</v8:content> внутри <Title> не
        // должно попасть в обработчики событий.
        let handlers = parse_form_xml(SAMPLE).unwrap();
        assert!(!handlers.iter().any(|h| h.handler.contains("Форма")));
    }

    #[test]
    fn returns_empty_for_missing_file() {
        let p = std::path::Path::new("/non/existent.xml");
        assert!(parse_form_file(p).unwrap().is_empty());
    }

    #[test]
    fn handler_of_form_element_keeps_owner() {
        // Раскладка как в реальной выгрузке: поле лежит внутри нескольких
        // групп оформления, обработчики формы — в корневом <Events>.
        let xml = r#"<?xml version="1.0"?>
<Form>
  <Events>
    <Event name="OnOpen">ПриОткрытии</Event>
  </Events>
  <ChildItems>
    <Page name="ГруппаТовары" id="18">
      <ChildItems>
        <InputField name="СуммаДокумента" id="1312">
          <ContextMenu name="СуммаДокументаКонтекстноеМеню" id="1313"/>
          <Events>
            <Event name="OnChange">СуммаДокументаПриИзменении</Event>
          </Events>
        </InputField>
      </ChildItems>
    </Page>
  </ChildItems>
</Form>
"#;
        let handlers = parse_form_xml(xml).unwrap();
        assert_eq!(handlers.len(), 2);
        let form_level = handlers.iter().find(|h| h.event == "ПриОткрытии").unwrap();
        assert_eq!(form_level.element, None);
        let field = handlers.iter().find(|h| h.event == "ПриИзменении").unwrap();
        // Владелец — само поле, а не страница-контейнер.
        assert_eq!(field.element.as_deref(), Some("СуммаДокумента"));
    }

    #[test]
    fn form_events_translated_in_both_dump_formats() {
        // Имя события переводится, имя процедуры остаётся как в модуле.
        let xml = r#"<?xml version="1.0"?>
<Form>
  <Events>
    <Event name="OnCreateAtServer">ПриСозданииНаСервере</Event>
    <Event name="БезымянноеСобытие">Обработчик</Event>
  </Events>
</Form>
"#;
        let handlers = parse_form_xml(xml).unwrap();
        assert!(handlers.iter().any(|h| h.event == "ПриСозданииНаСервере"));
        // Неизвестное имя проходит без изменений — ничего не теряем.
        assert!(handlers.iter().any(|h| h.event == "БезымянноеСобытие"));
        // Идентификатор события внешней компоненты русского имени не имеет.
        assert_eq!(
            form_event_to_russian("9cc34712-da5f-4faa-a653-343d2085fbe8"),
            "9cc34712-da5f-4faa-a653-343d2085fbe8"
        );
        // Имена, сверенные по конфигурациям: приставки «При» у события
        // изменения текста нет, у дополнительной расшифровки — свой порядок
        // слов, а «Tuning» в конфигураторе называется «Регулирование».
        assert_eq!(
            form_event_to_russian("EditTextChange"),
            "ИзменениеТекстаРедактирования"
        );
        assert_eq!(
            form_event_to_russian("AdditionalDetailProcessing"),
            "ОбработкаДополнительнойРасшифровки"
        );
        assert_eq!(form_event_to_russian("Tuning"), "Регулирование");
    }

    #[test]
    fn handlers_json_omits_element_for_form_level() {
        let handlers = vec![
            FormHandler {
                event: "OnOpen".into(),
                handler: "ПриОткрытии".into(),
                element: None,
            },
            FormHandler {
                event: "OnChange".into(),
                handler: "СуммаПриИзменении".into(),
                element: Some("Сумма".into()),
            },
        ];
        let json = handlers_to_json(&handlers).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert!(parsed[0].get("element").is_none());
        assert_eq!(parsed[1]["element"], "Сумма");
    }

    #[test]
    fn empty_events_block_yields_empty_vec() {
        let xml = r#"<?xml version="1.0"?>
<Form>
  <Events />
</Form>
"#;
        assert!(parse_form_xml(xml).unwrap().is_empty());
    }
}
