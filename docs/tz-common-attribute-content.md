# ТЗ: состав общих реквизитов в `data_links` и проверка `exchange_plan_content` на MDM

Источник: `C:\Rusin\mdm\docs\017\Фидбек.md`, раздел «Скиллы», пункт `1c-code-index` (задача 017
MDM, 09.10.2026). Правка описания скилла `1c-code-index` — в ТЗ контура
`C:\Rusin\1c-agent-contour\ТЗ_фидбек_MDM_017_скиллы_и_правила.md`, §12; здесь — индексатор.

## Факт

Задача включала три справочника в механизм MDM: составы общих реквизитов (`MDM_Key`,
`нсиРучноеИзменение`, `нсиЗаявка`), определяемых типов и плана обмена. Индекс ответил на «кто
использует объект» и про определяемые типы, но:

1. состава общих реквизитов в `data_links` нет — подходящего `link_kind` не существует;
2. про `exchange_plan_content` исполнитель пишет, что он «оказался про реквизиты плана, а не про
   его состав», и состав плана `нсиСинхронизацияДанныхЧерезСервисыИнтеграции` нашёл перебором
   дерева выгрузки.

## Что проверено

- `crates/bsl-extension/src/index_extras/full_scan.rs`: рёбра `subsystem_content`,
  `exchange_plan_content` (433–473, файлы `ExchangePlans/<Имя>/Ext/Content.xml`),
  `defined_type_content`, `functional_option_content` (547+). Для `CommonAttributes/*.xml` рёбер
  нет — п. 1 фидбека подтверждён.
- `xml/metadata_refs.rs:103`, `parse_exchange_plan_content_xml`, читает `<Item><Metadata>` —
  формат MDM (`ExchangePlanContent`, `version="2.20"`, `<Item><Metadata>Catalog.X</Metadata>
  <AutoRecord>Deny</AutoRecord>`) ему соответствует. П. 2 фидбека кодом **не** подтверждается:
  вероятнее старый индекс (бинарник до появления рёбер) или запрос по `from_object` вместо
  `to_object`. Индекса MDM на диске при подготовке ТЗ не нашлось — проверка ниже, п. 2.

## Что менять

### 1. Рёбра `common_attribute_content`

- Источник: `CommonAttributes/<Имя>.xml`, `<Properties><Content><xr:Item>` →
  `<xr:Metadata>Catalog.X</xr:Metadata>`, `<xr:Use>Use|DontUse</xr:Use>` (и `<xr:ConditionalSeparation>`
  не нужен). Формат EDT (`.mdo`, `content`) — так же, как у остальных видов в `edt_mdo.rs`.
- Ребро: `from_object = CommonAttribute.<Имя>`, `to_object = <объект>`, `link_kind =
  common_attribute_content`. Пишутся только элементы с `Use = Use`; `DontUse` — не состав.
  `AutoUse` общего реквизита (`<AutoUse>Use</AutoUse>` — реквизит у **всех** объектов, кроме
  `DontUse`) — отдельной строкой: ребро на `*All` с `from_path = "auto"`, чтобы запрос «в каком
  общем реквизите объект» не терял автоприменяемые.
- Схема (`schema.rs:478+`), описание `bsl_sql` (`tools/bsl_sql.rs:88`), `find_references`
  (`tools/find_references.rs:8`) — новый вид в перечнях. Версия схемы индекса — повысить, если
  миграция требует переиндексации; `CHANGELOG`/`CHANGELOG_EN`.

### 2. Проверка `exchange_plan_content` на выгрузке MDM

Построить индекс `C:\Rusin\mdm\src\cf` текущим бинарником и выполнить:

```sql
SELECT from_object, count(*) FROM data_links
WHERE link_kind = 'exchange_plan_content'
  AND from_object = 'ExchangePlan.нсиСинхронизацияДанныхЧерезСервисыИнтеграции';
SELECT from_object FROM data_links
WHERE link_kind = 'exchange_plan_content' AND to_object = 'Catalog.КРБ_ВидыСудов';
```

Ожидается: первый запрос — число элементов `Ext/Content.xml` плана (сверить с
`grep -c "<Metadata>"`), второй — `ExchangePlan.нсиСинхронизацияДанныхЧерезСервисыИнтеграции`
(после задачи 017). Совпало — дефекта индексатора нет, итог одной строкой в `CHANGELOG` не
пишется, а передаётся в ТЗ контура (§12: рецепт запроса в скилле). Не совпало — найти причину
(обход каталога, вложенный `Ext`, кодировка BOM) и исправить с тестом на фикстуре формата MDM.

## Проверяемый результат

- Тест в `index_extras/tests.rs`: фикстура общего реквизита с тремя элементами `<Content>` (`Use`,
  `Use`, `DontUse`) — два ребра `common_attribute_content`; `AutoUse = Use` — ребро на `*All`.
- На индексе MDM: `SELECT from_object FROM data_links WHERE link_kind='common_attribute_content'
  AND to_object='Catalog.КРБ_ВидыСудов'` возвращает `CommonAttribute.MDM_Key`,
  `CommonAttribute.нсиРучноеИзменение`, `CommonAttribute.нсиЗаявка`.
- П. 2 — результат двух запросов записан в ответ сопровождающему контура.

## Поставка

Новый бинарник — в `C:\Rusin\1c-agent-contour\bin\bsl-indexer.exe` штатным `publish_bins`
(версия `1.0.1-ka.8` или следующая), затем переиндексация в MDM.
