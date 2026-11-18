# Руководство по JSQNL v0.1

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNL — локальная встраиваемая база данных на Rust. Для запросов используется CLI `jsqnl`, для импорта — `in2jsqnl`. Тот же движок доступен как библиотека Rust.

## Сборка и запуск

Установите Rust 1.89 или новее, Cargo и системные библиотеки разработки SQLite. Выполняйте команды из корневого каталога репозитория.

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked
```

Скопируйте файл примера перед изменением данных. Команда `exec` выполняет запросы к постоянной базе и выводит результаты в JSON Lines. Команда `run` выполняет смешанный сценарий в памяти. Для создания пустой базы используйте `jsqnl init db.jsqnl`.

```sh
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl run examples/demo.jsqnl
```

## Формат файлов

Базовый файл `.jsqnl` содержит строки схемы и объекты JSON в кодировке UTF-8. Каждый объект занимает одну физическую строку и относится к последней объявленной схеме. Необязательный заголовок `-- JSQNL: 1` размещается в первой строке.

```sql
-- JSQNL: 1
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
```

Поддерживаются типы `INT` (знаковое 64-битное целое), `FLOAT`, `TEXT`, `BOOL`, `JSON` и `VECTOR(n)`. Вектор должен содержать ровно `n` конечных чисел. Поля, допускающие пустые значения, можно опустить или указать как `null`. Для таблицы можно задать один первичный ключ типа `INT` или `TEXT`. Правила кодирования описаны в [спецификации формата](../../FORMAT.md).

## Запросы и обновления

Для фильтрации используйте `WHERE`, `IN`, `BETWEEN` и `IS NULL`. Для сортировки и постраничной выдачи используйте `ORDER BY`, `LIMIT` и `OFFSET`. Для подсчёта по группам объединяйте `GROUP BY` с `COUNT(*)`. `INNER JOIN` и `LEFT JOIN` сопоставляют по равенству по одному квалифицированному столбцу из каждой из двух таблиц одной базы.

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC LIMIT 5;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

`THE 1` и `_line` обращаются к логическому идентификатору строки в таблице. Он независим от номера физической строки и первичного ключа. `WHO =` заменяет значение. `WHO +` складывает числа, соединяет строки или дополняет массив JSON. `FUSE users THE 1 INTO THE 2 KEEP TARGET;` заполняет отсутствующие и пустые поля целевой строки, сохраняет её ключ и идентификатор и удаляет исходную строку.

```sql
RESONATE embedding FROM users WITH [1,0,0] LIMIT 5;
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`RESONATE` упорядочивает подходящие ненулевые векторы по косинусному сходству. `DECAY` умножает поле `FLOAT` или каждую компоненту вектора на коэффициент от 0 до 1.

## Импорт JSON и JSONL, чтение Parquet

IN2JSQNL объединяет одно поддерживаемое определение SQL `CREATE TABLE` с объектом JSON, массивом объектов или данными JSONL. Формат можно выбрать через `--format json` или `--format jsonl`, а таблицу в SQL-файле — через `--table`. Составные ключи, значения по умолчанию и внешние ключи не поддерживаются.

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

`scan` читает файлы Parquet с одинаковой схемой в порядке имён. Требуется установленный PyArrow. При необходимости укажите интерпретатор Python через `JSQNL_PYTHON`. Имя таблицы берётся из имени каталога. Для каталога `data` используется имя родительского каталога. За один запуск выполняется один запрос `SELECT`.

```sh
./target/release/jsqnl scan /path/to/enwiki/data 'SELECT identifier FROM enwiki LIMIT 5;'
```

## Хранение и эксплуатационные ограничения

Для базового файла без журнала обновлений можно построить дисковый индекс смещений. Он ускоряет поиск по равенству `_line` или первичного ключа и `COUNT(*)` без фильтра. `EXPLAIN ANALYZE` выполняет `SELECT` и сообщает способ доступа, объём чтения и время. `check` проверяет базу и выводит состояние индекса.

```sh
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
./target/release/jsqnl check users.jsqnl
./target/release/jsqnl fork users.jsqnl experiment
```

Успешный пакет записи добавляет транзакцию в соседний файл `.log`. Храните базовый файл и журнал вместе. `fork` создаёт согласованную копию в новом каталоге. При ошибке потокового чтения часть строк уже может быть выведена. Перед использованием результатов проверьте код завершения.

Крупные соединения, группировки и сортировки могут использовать временное место на диске. Бюджеты задаются через `JSQNL_JOIN_MEMORY_BYTES`, `JSQNL_GROUP_MEMORY_BYTES` и `JSQNL_SORT_MEMORY_BYTES`. `JSQNL_TEMP_LIMIT_BYTES` задаёт объём временного места на запрос (по умолчанию 8 ГиБ). Бюджеты памяти приблизительны. Широкие промежуточные строки и проверка первичных ключей без индекса могут увеличить расход памяти. База с журналом обновлений воссоздаётся в памяти.

Для аутентификации журнала укажите через `JSQNL_JOURNAL_KEY_FILE` путь к защищённому файлу с ключом длиной 32 байта. Версия 3 аутентифицирует подтверждённые транзакции и привязывает их к каноническому пути базы. Команда `fork` повторно подписывает копию для нового пути. Чтобы обнаружить потерю последних подтверждённых транзакций, сохраните последнее состояние отдельно в доверенном месте.

## Rust API

`jsqnl::execute` принимает путь к базе, SQL и функцию обратного вызова для строк JSON, а возвращает `QueryReport`. Пакет записи фиксируется до вызова функции обработки результатов.

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{}", report.rows_output);
```

## Лицензия и корпоративные запросы

JSQNL распространяется по [лицензии MIT](../../LICENSE). Её условия также действуют при корпоративном и коммерческом использовании. По вопросам внедрения, сопровождения или отдельных соглашений обращайтесь к разработчикам.
