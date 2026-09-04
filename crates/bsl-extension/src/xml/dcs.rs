// Парсер схем компоновки данных (СКД) — `Templates/<Имя>/Ext/Template.xml`
// с корнем `<DataCompositionSchema>`.
//
// Назначение одно: достать тексты запросов наборов данных. Запрос СКД
// обращается к таблицам конфигурации так же, как запрос в модуле
// (`Документ.Заказ`, `РегистрНакопления.Остатки.Обороты`), но лежит не в
// `.bsl`, а в макете, и обратный индекс использований его не видел: отчёт,
// читающий документ, в карту влияния документа не попадал.
//
//   <DataCompositionSchema …>
//     <dataSet xsi:type="DataSetQuery">
//       <query>ВЫБРАТЬ … ИЗ Документ.Заказ КАК З</query>
//     </dataSet>
//     <dataSet xsi:type="DataSetUnion">
//       <item xsi:type="DataSetQuery"><query>…</query></item>
//
// Берём каждый `<query>` на любой глубине — наборы-объединения вкладывают
// запросы в `<item>`. Содержимое макетов другого рода (табличные документы,
// двоичные данные) сюда не подаётся: вызывающий проверяет корень по первым
// байтам файла (`is_dcs_template`), макеты печатных форм бывают по десятки
// мегабайт и читать их целиком незачем.

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;

/// Похоже ли начало файла на схему компоновки данных.
pub fn is_dcs_template(head: &str) -> bool {
    head.contains("<DataCompositionSchema")
}

/// Тексты запросов схемы: номер строки файла, на которой открылся тег
/// `<query>` (1-based), и текст как в файле — с ведущими переводами строк,
/// чтобы строки внутри текста отсчитывались от той же точки.
pub fn parse_dcs_queries(content: &str) -> Result<Vec<(usize, String)>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);

    let mut out: Vec<(usize, String)> = Vec::new();
    let mut buf = Vec::new();
    // Строка открывающего тега текущего `<query>`; None — мы не внутри него.
    let mut query_line: Option<usize> = None;
    let mut depth_in_query = 0usize;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if query_line.is_some() {
                    depth_in_query += 1;
                } else if local == "query" {
                    query_line = Some(line_at(content, reader.buffer_position() as usize));
                    depth_in_query = 0;
                }
            }
            Ok(Event::End(_)) => {
                if query_line.is_some() {
                    if depth_in_query == 0 {
                        query_line = None;
                    } else {
                        depth_in_query -= 1;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(line) = query_line {
                    let text = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    push_text(&mut out, line, &text);
                }
            }
            Ok(Event::CData(c)) => {
                if let Some(line) = query_line {
                    let text = String::from_utf8_lossy(c.into_inner().as_ref()).into_owned();
                    push_text(&mut out, line, &text);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "DataCompositionSchema: ошибка парсинга на позиции {}: {}",
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

/// Текст одного `<query>` может прийти несколькими узлами (сущности, CDATA) —
/// склеиваем в один запрос той же строки.
fn push_text(out: &mut Vec<(usize, String)>, line: usize, text: &str) {
    match out.last_mut() {
        Some((l, q)) if *l == line => q.push_str(text),
        _ => out.push((line, text.to_string())),
    }
}

/// Номер строки (1-based), на которой лежит байтовая позиция `pos`.
fn line_at(content: &str, pos: usize) -> usize {
    let end = pos.min(content.len());
    content.as_bytes()[..end].iter().filter(|&&b| b == b'\n').count() + 1
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name).into_owned();
    match s.find(':') {
        Some(idx) => s[idx + 1..].to_string(),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DCS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DataCompositionSchema xmlns="http://v8.1c.ru/8.1/data-composition-system/schema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
	<dataSource>
		<name>ИсточникДанных</name>
	</dataSource>
	<dataSet xsi:type="DataSetQuery">
		<name>Основной</name>
		<query>ВЫБРАТЬ
	З.Ссылка КАК Ссылка
ИЗ
	Документ.Заказ КАК З</query>
	</dataSet>
	<dataSet xsi:type="DataSetUnion">
		<name>Объединение</name>
		<item xsi:type="DataSetQuery">
			<query>
ВЫБРАТЬ Т.Номенклатура ИЗ Документ.Реализация.Товары КАК Т
ГДЕ Т.Количество &gt; 0</query>
		</item>
	</dataSet>
</DataCompositionSchema>
"#;

    #[test]
    fn extracts_all_queries_with_lines() {
        let q = parse_dcs_queries(DCS).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].0, 8, "строка открывающего <query> первого набора");
        assert!(q[0].1.starts_with("ВЫБРАТЬ"));
        assert!(q[0].1.contains("Документ.Заказ"));
        // Второй запрос начинается с перевода строки — текст сохранён как в файле.
        assert_eq!(q[1].0, 16);
        assert!(q[1].1.starts_with('\n'));
        // Сущность `&gt;` раскрыта.
        assert!(q[1].1.contains("> 0"));
    }

    #[test]
    fn lines_inside_query_text_follow_file() {
        let q = parse_dcs_queries(DCS).unwrap();
        let usages = crate::code_usages::extract_query_usages(&q[1].1, q[1].0, "dcs_query");
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].object_ref, "Document.Реализация");
        assert_eq!(usages[0].member_path.as_deref(), Some("Товары"));
        // Путь на второй физической строке текста (после ведущего перевода) → 17.
        assert_eq!(usages[0].line, 17);
    }

    #[test]
    fn head_check_distinguishes_dcs_from_spreadsheet() {
        assert!(is_dcs_template(
            "<?xml version=\"1.0\"?>\n<DataCompositionSchema xmlns=\"http://v8.1c.ru/8.1/data-composition-system/schema\">"
        ));
        assert!(!is_dcs_template(
            "<?xml version=\"1.0\"?>\n<Template xmlns=\"http://v8.1c.ru/8.2/data/spreadsheet\">"
        ));
    }

    #[test]
    fn schema_without_queries_is_empty() {
        let q = parse_dcs_queries("<DataCompositionSchema><dataSet xsi:type=\"DataSetObject\"><name>Н</name><objectName>Т</objectName></dataSet></DataCompositionSchema>").unwrap();
        assert!(q.is_empty());
    }
}
