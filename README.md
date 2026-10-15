# DBMan

Десктопный клиент для разных баз данных в духе DataGrip, на Rust + [egui](https://github.com/emilk/egui) с виджетами [shadcn_egui](https://github.com/georgebigscar-spec/shadcn_egui).

Поддерживаются **PostgreSQL**, **MySQL / MariaDB** и **SQLite**; новые СУБД подключаются через общий трейт драйвера.

![SQL-консоль](crates/dbman/tests/snapshots/console.png)

| Редактирование данных | Структура объекта |
|---|---|
| ![editing](crates/dbman/tests/snapshots/data_editing.png) | ![structure](crates/dbman/tests/snapshots/object_structure.png) |

## Что уже умеет

- **Подключения**: создание, редактирование, копирование, удаление, проверка соединения; SSL-режимы `disable / prefer / require / verify-full`; для SQLite выбор файла. Настройки хранятся в `settings.json` в каталоге конфигурации пользователя (пароль сохраняется, только если отмечено «Save password»).
- **Database Explorer**: дерево подключение → база → схема (у PostgreSQL) → группы объектов → объекты → колонки, индексы, внешние ключи. Всё грузится лениво при раскрытии.
  - PostgreSQL: таблицы, представления, материализованные представления, внешние таблицы, последовательности, функции и процедуры (с перегрузками), триггеры, типы (enum, domain, composite).
  - MySQL / MariaDB: таблицы, представления, последовательности (MariaDB), функции, процедуры, триггеры.
  - SQLite: таблицы, представления, триггеры.
- **SQL-консоль** с подсветкой синтаксиса. У каждой консоли своя сессия (свои транзакции), как в DataGrip. `Ctrl+Enter` выполняет выражение под курсором или выделение, `Ctrl+Shift+Enter` весь скрипт; у каждого выражения свой результат.
- **Просмотр данных** таблиц и представлений: страницы, фильтр `WHERE`, `ORDER BY`, быстрый фильтр и сортировка в таблице, `NULL` выделен.
- **Редактирование данных** в таблицах: двойной клик по ячейке, `Set NULL` из контекстного меню, `+ Row` и `− Delete` для строк. Изменения подсвечены, пока их не отправили; `Preview SQL` показывает итоговые `INSERT / UPDATE / DELETE`, `Submit` выполняет их в одной транзакции в отдельной сессии. Строки находятся по первичному ключу (без него по всем колонкам), и если `UPDATE` или `DELETE` задел не ровно одну строку, всё откатывается.
- **Экспорт** результата консоли или страницы таблицы в CSV, TSV, JSON, SQL `INSERT` или Markdown: в буфер обмена или в файл.
- **Структура и DDL** любого объекта: колонки, индексы, внешние ключи и `CREATE`-выражение.
- Светлая и тёмная темы.

Горячие клавиши: `Ctrl+T` новая консоль, `Ctrl+W` закрыть вкладку, `F4` открыть выбранный объект, двойной клик по таблице открывает данные.

## Запуск

```sh
cargo run --release
```

На Linux для диалога выбора файла нужен xdg-desktop-portal (есть в обычных GNOME/KDE).

## Архитектура

```
crates/
  dbman-core/     модели и трейт Connection, без GUI и драйверов
  dbman-drivers/  реализации: postgres (tokio-postgres), mysql (mysql_async), sqlite (rusqlite)
  dbman/          приложение на eframe + shadcn_egui
```

- **`dbman-core`**: `ConnectionConfig` и `DbKind`; трейт `Connection` (список баз, схем, объектов, структура таблицы, DDL, выполнение SQL); модели результата (`QueryResult`, `Cell`) и схемы (`DbObject`, `TableDetails`); токенизатор SQL для подсветки и разбиения скрипта на выражения с учётом диалекта (`$$`-строки PostgreSQL, обратные кавычки и `#`-комментарии MySQL), экранирование идентификаторов и строк.
- **`dbman-drivers`**: по модулю на СУБД и функция `connect(&ConnectionConfig)`. Чтобы добавить СУБД, нужен вариант в `DbKind`, реализация `Connection` и строка в `connect`.
- **`dbman`**: интерфейс никогда не ждёт базу. Запросы выполняются на tokio-рантайме, результаты приходят событиями через канал и забираются каждый кадр (`events.rs`). Дерево (`explorer.rs`), вкладки (`tabs.rs`), сетка результатов (`grid.rs`), редактор SQL (`sql_editor.rs`), диалог подключения (`connection_dialog.rs`).

## Тесты

```sh
cargo test --workspace
```

Тесты драйверов для SQLite идут всегда; для PostgreSQL и MySQL нужны адреса серверов:

```sh
DBMAN_TEST_POSTGRES=postgres://postgres:secret@127.0.0.1:5432/postgres \
DBMAN_TEST_MYSQL=mysql://root:secret@127.0.0.1:3306/dbman_test \
cargo test --workspace
```

`crates/dbman/tests/ui.rs` прогоняет окно целиком на примерной базе SQLite. С `DBMAN_SCREENSHOTS=1 UPDATE_SNAPSHOTS=1` он перерисовывает скриншоты в `crates/dbman/tests/snapshots/` (нужен wgpu-адаптер; без GPU подойдёт `mesa-vulkan-drivers`).

## Дальше

1. Создание и изменение объектов через формы: базы, схемы, таблицы (колонки, ключи, индексы, внешние ключи), представления; удаление и переименование; DDL генерируется под диалект.
2. Резервное копирование и восстановление под каждую СУБД: `pg_dump` / `pg_restore` (форматы plain/custom/directory/tar, выбор схем и таблиц, `--clean`, параллельность), `mysqldump` / `mariadb-dump` (`--single-transaction`, процедуры, триггеры, события), SQLite через online backup API и `VACUUM INTO`, с журналом и прогрессом.
3. Автодополнение в консоли, история запросов, план выполнения (`EXPLAIN`), отмена запроса, SSH-туннели, хранение паролей в системном keychain.

## Лицензия

MIT
