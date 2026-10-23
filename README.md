# DBMan

**English** · [Русский](README.ru.md)

A desktop client for many databases in the spirit of DataGrip, written in Rust with [egui](https://github.com/emilk/egui) and the [shadcn_egui](https://github.com/georgebigscar-spec/shadcn_egui) widgets.

It supports **PostgreSQL**, **MySQL / MariaDB**, **SQL Server**, **ClickHouse**, **SQLite** and **DuckDB**, and the databases on the PostgreSQL and MySQL protocols: **CockroachDB**, **YugabyteDB**, **TimescaleDB**, **Supabase**, **Neon**, **TiDB**, **PlanetScale** and **SingleStore**. New engines plug in through a common driver trait.

![SQL console](crates/dbman/tests/snapshots/console.png)

| Editing data | Object structure |
|---|---|
| ![editing](crates/dbman/tests/snapshots/data_editing.png) | ![structure](crates/dbman/tests/snapshots/object_structure.png) |

| Backup | Background tasks |
|---|---|
| ![backup](crates/dbman/tests/snapshots/backup_dialog.png) | ![tasks](crates/dbman/tests/snapshots/background_tasks.png) |

| Data import | Data export |
|---|---|
| ![import](crates/dbman/tests/snapshots/import_dialog.png) | ![export](crates/dbman/tests/snapshots/export_dialog.png) |

![ER diagram](crates/dbman/tests/snapshots/er_diagram.png)

| Schema comparison | Data comparison |
|---|---|
| ![compare structure](crates/dbman/tests/snapshots/compare_structure.png) | ![compare data](crates/dbman/tests/snapshots/compare_data.png) |

| Copying tables to another connection | Russian interface |
|---|---|
| ![copy](crates/dbman/tests/snapshots/copy_tables.png) | ![russian](crates/dbman/tests/snapshots/russian.png) |

![Test data generator](crates/dbman/tests/snapshots/generate_dialog.png)

## Features

- **Connections**: create, edit, duplicate, delete and test them.
  - SSL modes `disable / prefer / require / verify-full`.
  - SQLite and DuckDB take a file (DuckDB also takes `:memory:` and a folder, below).
  - **SSH tunnels** (the `SSH` tab): a server connection can go through an SSH server, signing in with a password or a private key file (OpenSSH, PEM or PKCS#8, with an optional passphrase). The database's host and port are reached from the SSH server, so `localhost` there is the SSH server itself. The sessions of a connection share one SSH login, and backups, restores and exports go through it too. Host keys are checked against `~/.ssh/known_hosts` and DBMan's own `known_hosts` next to its settings: a new server's key is remembered on the first connection, and a changed key is refused. Through a tunnel DBMan connects to `127.0.0.1`, which does not match the server certificate's host name, so use SSL `require` rather than `verify-full`.

![SSH tunnel](crates/dbman/tests/snapshots/ssh_tunnel.png)
  - Settings live in `settings.json` in the user's config directory. A password is saved only when "Save password" is ticked, and it goes to the system keychain: Keychain on macOS, Credential Manager on Windows, and the Secret Service (GNOME Keyring, KWallet) on Linux. Each connection is one item named `DBMan`, holding its database and SSH passwords. Without a keychain, or with `DBMAN_KEYCHAIN=off`, passwords stay in `settings.json` (readable only by its owner on Linux and macOS). Passwords already in `settings.json` move to the keychain on the next save.
- **Compatible databases**: the `Database type` list has, after PostgreSQL, CockroachDB, YugabyteDB, TimescaleDB, Supabase and Neon, and after MySQL / MariaDB, TiDB, PlanetScale and SingleStore. They connect with the PostgreSQL or MySQL driver; picking one fills in its usual port (CockroachDB 26257, YugabyteDB 5433, TiDB 4000), user and database, and `require` TLS for the hosted services (Timescale Cloud, Supabase, Neon, PlanetScale). The explorer and tabs show the product's own badge.
  - CockroachDB and TiDB are tested like PostgreSQL and MySQL themselves: the explorer, structure and DDL, data editing, the table editor, cancelling queries, read-only mode, execution plans and the sessions monitor.
  - CockroachDB sessions come from `crdb_internal.cluster_sessions` and stop with `CANCEL QUERIES` / `CANCEL SESSION`; its plans are read from its text `EXPLAIN` and `EXPLAIN ANALYZE`. Backups through `pg_dump` do not work with CockroachDB, so its menus have no `Back up…` / `Restore…`.
  - TiDB plans are read from its own `EXPLAIN` tree. TiDB has no read-only transactions, so a read-only TiDB connection relies on DBMan's own check.

![Compatible databases](crates/dbman/tests/snapshots/compatible_databases.png)
- **A folder of data files as a database**: pick DuckDB and `Folder…` in the connection dialog, or type a folder's path. The folder becomes a database of its name, with a view over every CSV, TSV, Parquet and JSON (or JSON Lines) file in it, compressed ones (`.gz`, `.zst`) too. Files at the top are in `main`, and those of each subfolder in a schema named after it; hidden files and folders are skipped. Nothing is imported: queries read the files as they are now, through DuckDB's `read_csv_auto`, `read_parquet` and `read_json_auto`, and you can join them, filter them and export the results. New and deleted files show up on the next refresh. A view's comment is the file's path in the folder. The database itself lives in memory, so tables made in a console are gone when the last session closes, and there is no backup for it.

![Folder of files](crates/dbman/tests/snapshots/folder_of_files.png)
- **Connection options** (the `Options` tab of the connection dialog):
  - **Folder**: connections with the same folder are grouped under it in the explorer, folders first. The folders already in use are offered below the field.
  - **Color**: a mark on the connection's row and tabs, and a line over its tabs' content, so that production stands out (for example in red).
  - **Read-only**: only statements that read run. Anything else (`INSERT`, `UPDATE`, `DELETE`, DDL, `SELECT … INTO`, a `WITH` that changes data, `EXPLAIN ANALYZE` of a change, procedure calls) is refused before it reaches the server, in consoles and everywhere else in DBMan. The server is asked to enforce it as well where it can: PostgreSQL sessions start with `default_transaction_read_only` (so a `SELECT` of a function that writes fails too), MySQL sessions with `SET SESSION TRANSACTION READ ONLY`, SQLite with `PRAGMA query_only`, and ClickHouse queries with `readonly=2`. Menu items that change the database (create, drop, rename, truncate, edit tables, import, generate data, restore) are hidden, and the data view does not edit rows. The row in the explorer says `read-only`.
  - **Confirm changes in consoles**: before a run with a statement that changes data or structure, the console shows the first such statement and asks.

![Read-only connection](crates/dbman/tests/snapshots/read_only.png)

| Connection options | Confirming changes |
|---|---|
| ![options](crates/dbman/tests/snapshots/connection_options.png) | ![confirm](crates/dbman/tests/snapshots/confirm_changes.png) |
- **Database Explorer**: a tree of connection → database → schema (on PostgreSQL) → object groups → objects → columns, indexes and foreign keys. Everything loads lazily as you expand it.
  - PostgreSQL: tables, views, materialized views, foreign tables, sequences, functions and procedures (with overloads), triggers, and types (enum, domain, composite).
  - MySQL / MariaDB: tables, views, sequences (MariaDB), functions, procedures and triggers.
  - SQL Server: databases → schemas → tables, views, procedures, functions, triggers, sequences and user-defined types. Comments come from `MS_Description`.
  - ClickHouse, over the HTTP interface (port 8123, or 8443 with SSL `require` / `verify-full`):
    - databases → tables (engine shown next to the name), views, materialized views and dictionaries;
    - the primary key and data-skipping indexes appear in the structure;
    - arrays, maps and tuples show as JSON, and big integers keep their precision;
    - ClickHouse tables open read-only; change data in the console.
  - SQLite: tables, views and triggers.
  - DuckDB: attached databases (`ATTACH`) → schemas → tables, views, sequences, macros and types. Lists, structs and maps show the way DuckDB prints them.
- **SQL console** with syntax highlighting.
  - Every console has its own session, so its own transactions, as in DataGrip.
  - **Manual commit**: the `Auto-commit` / `Manual commit` switch in the console toolbar. In manual mode the first run opens a transaction (`BEGIN`, `START TRANSACTION` on MySQL, `BEGIN TRANSACTION` on SQL Server), and the changes wait for `Commit` or `Rollback`, which appear next to a `transaction open` mark. A `BEGIN`, `COMMIT` or `ROLLBACK` typed in the script is followed too, in either mode, and so are MySQL's implicit commits before DDL. While a transaction is open the mode and the database cannot be switched, and closing the console asks first and rolls it back. `Explain analyze` runs inside the transaction as well. ClickHouse has no transactions, so it has no switch.

![Manual commit](crates/dbman/tests/snapshots/transaction.png)
  - `Ctrl+Enter` runs the statement under the cursor or the selection. `Ctrl+Shift+Enter` runs the whole script.
  - Every statement gets its own result.
  - Autocomplete of tables, views, schemas, columns of the tables and aliases in the query, functions and keywords. It opens as you type a name or a `.` and on `Ctrl+Space`; `Tab` or `Enter` takes the selected item, and names are quoted where the engine needs it. Tables and columns load from the server on first use.

![Autocomplete](crates/dbman/tests/snapshots/autocomplete.png)

  - Snippets: at the start of a statement, `sel`, `selw`, `cnt`, `ins`, `upd` and `del` expand to `SELECT * FROM`, `SELECT * FROM … WHERE`, `SELECT count(*) FROM`, `INSERT INTO … () VALUES ()`, `UPDATE … SET … WHERE` and `DELETE FROM … WHERE`, with the cursor where the table name goes.
  - Find and replace: `Ctrl+F` opens a bar over the script with the selection to look for, and `Ctrl+R` (or `Ctrl+H`) also opens the replace row. Matches are highlighted, `Enter` and `Shift+Enter` go to the next and previous one, `Aa` matches case and `W` whole words only. `Replace` changes the current match and goes to the next, and `Replace all` changes them all. `Escape` closes the bar and puts the cursor on the current match.

![Find and replace](crates/dbman/tests/snapshots/find_replace.png)

- **Query parameters**: `:name`, `${name}` and, on MySQL, SQL Server, SQLite and DuckDB, `?` in a statement are asked for before it runs (`?` is an operator in PostgreSQL and ClickHouse). Strings, comments, `::` casts and ClickHouse's `{name:Type}` are left alone. The dialog shows the statement with the values in place; they go in as written, so text needs quotes (`'Berlin'`) and `NULL` means none. Values are remembered by name for the next time, and `Explain` asks for them too.

![Query parameters](crates/dbman/tests/snapshots/query_parameters.png)

- **Query history**: every statement run in a console is kept with its time, duration and result (the last 2000, in `history.jsonl` next to the settings). `History` in the console shows those of its connection, newest first, with a search; a click adds the statement to the script.
- **Script files**: `Script` in the console opens a `.sql` file, saves it (`Ctrl+S`) or saves it under another name, and lists recent files. The tab shows the file name, with `•` while there are unsaved changes.
- **Tabs come back after a restart**: consoles (with their text, even unsaved, their database, file and commit mode), data, structure, diagram and session tabs reopen as they were, and the tab that was in front is in front again. Nothing connects until a tab is used. They are kept in `workspace.json` next to the settings, readable only by you; table editors, comparisons and copies are not kept, and tabs of a deleted connection are dropped.

![Query history](crates/dbman/tests/snapshots/query_history.png)

- **Execution plan**: `Explain` in the console shows the plan of the statement under the cursor as a tree with the estimated rows and cost, without running it. `Explain analyze` (PostgreSQL and DuckDB) runs the statement and adds the actual rows and times; a bar shows each step's share of the time, or of the cost. `Copy plan` copies the engine's own output. Each engine is asked its own way: `EXPLAIN (FORMAT JSON)` in PostgreSQL and DuckDB, `EXPLAIN` in MySQL and MariaDB, `EXPLAIN QUERY PLAN` in SQLite, `EXPLAIN indexes = 1` in ClickHouse, and `SET SHOWPLAN_ALL ON` in SQL Server.

![Execution plan](crates/dbman/tests/snapshots/explain_plan.png)
- **SQL formatting**: `Format SQL` in the console (`Ctrl+Alt+L`) lays out the selection, or the whole script, one clause per line: select lists and `SET` assignments one item per line, `AND` / `OR` conditions on their own lines, subqueries and CTEs indented, `CREATE TABLE` one column per line, and keywords in upper case (on ClickHouse, whose names are case-sensitive, the case is kept). Other statements, such as procedure bodies, triggers and `GRANT`, are kept as written, and so are strings and comments. Only whitespace and the case of keywords change, so the formatted script means the same; a test runs queries as written and formatted on every engine and compares the results.

![SQL formatting](crates/dbman/tests/snapshots/format_sql.png)
- **Value panel**: `Value` above a result grid or table data opens a panel with the whole value of the clicked cell (a double click in console results opens it too). JSON objects and arrays are indented, XML gets one element per line, long text wraps, and bytes are shown as a hex dump with offsets and ASCII. PNG, JPEG, GIF, BMP and WebP images are recognized by their first bytes and drawn above the dump. The panel names the column, its type, the row and the size, and `Copy` copies the value as it is. Values over 1 MB are cut for display.

![Value panel](crates/dbman/tests/snapshots/value_panel.png)
- **Selected cells**: Ctrl+click adds or removes cells and Shift+click selects the rectangle between two cells, in the order the rows are shown. With more than one cell selected, a line under the grid shows the count, the NULLs, and for numbers the sum, average, minimum and maximum. `Copy` or Ctrl+C copies the selection as tab-separated text.

![Selected cells](crates/dbman/tests/snapshots/selection_stats.png)
- **Data view** of tables and views: pages, a `WHERE` filter, `ORDER BY`, quick filter and sort in the grid, and `NULL` stands out.
- **Data editing** in tables.
  - Double-click a cell to edit it. `Set NULL` is in the context menu, and `+ Row` and `− Delete` add and delete rows.
  - Changes stay highlighted until submitted. `Preview SQL` shows the resulting `INSERT / UPDATE / DELETE`.
  - `Submit` runs them in one transaction in a separate session.
  - Rows are found by primary key, or by all columns when there is none. If an `UPDATE` or `DELETE` touches anything other than exactly one row, everything rolls back.
- **Export** of the rows on screen to CSV, TSV, JSON, JSON Lines, SQL `INSERT` or Markdown, to the clipboard or a file, and to Excel or Parquet files.
  - Excel (`.xlsx`): numbers, dates, timestamps and booleans keep their cell types, the header row is bold and frozen with a filter, and past 1,048,575 rows a new sheet begins. The workbook is written as rows arrive.
  - Parquet: rows are gathered in a temporary DuckDB file and written with zstd. Columns keep their types (integers, decimals, dates, timestamps, booleans, UUIDs, bytes) wherever every value fits, and become text otherwise.
- **Whole-table export** (`Export data…` in a table or view menu, and `All rows to a file…` in the export menu of the console and the data view) to the same formats.
  - It runs as a background job in its own session.
  - The table is read page by page by primary key and written as it is read, so table size is not limited by memory.
  - In the data view, `WHERE` and `ORDER BY` apply.
  - Progress comes from `COUNT(*)`. `Cancel` stops the query on the server and deletes the unfinished file.
- **Data import** (`Import data…` in a table menu) from CSV / TSV, JSON, Excel and Parquet.
  - CSV / TSV: any delimiter, header row, quoted fields with line breaks, and BOM.
  - JSON: an array of objects, or JSON Lines.
  - Excel (`.xlsx`, `.xlsm`): pick the sheet; dates and times come out as `YYYY-MM-DD hh:mm:ss`, and formulas give their saved value. The sheet is read as it streams, never whole.
  - Parquet: read through the bundled DuckDB, so nothing else is needed.
  - The dialog shows the first rows of the file and matches columns by name (ignoring case and `_`) or by position. Any column can be skipped.
  - The file is read in its own thread, and rows go in as batches of multi-row `INSERT`s (up to 1000 rows on SQL Server).
  - Options: empty fields as `NULL`, delete the rows first, and everything in one transaction that rolls back on error. The error message names the rows it happened on.
  - Hex `0x…` in binary columns becomes bytes, so binary data survives an export and import round trip.
  - SQL Server turns on `IDENTITY_INSERT` when identity values are loaded, and PostgreSQL moves sequences past the largest loaded value.
- **New table from a file** (`New table from a file…` in a schema menu, or the database menu on engines without schemas).
  - The column types are guessed from the first 10,000 rows: whole numbers, decimals with their precision, floats, booleans, dates, timestamps, UUIDs, `0x…` bytes, and text otherwise. Codes with leading zeros such as `007` stay text.
  - Names and types can be changed before the import. The table is created first and dropped again if the import fails.
- **ER diagram** of a schema (`ER diagram` in the schema menu, or the database menu on engines without schemas).
  - Tables with their columns, primary keys (orange dot) and foreign keys (blue dot).
  - Links run from the foreign key column to the column it references.
  - A crow's foot marks the child side when one parent row has many child rows. A dashed line marks an optional link (the key allows `NULL`).
  - Tables in other schemas that are referenced have a dashed border.
  - The layout is automatic: parents to the left of children, ordered so that fewer lines cross, and unlinked tables in a grid below.
  - Drag tables with the mouse. Pan by dragging the background or with the wheel, and zoom with `Ctrl` + wheel.
  - `Fit` shows everything, and `Keys only` keeps only key columns. Search finds a table and highlights its links, and a double click opens its data.
  - Export to SVG, Mermaid (`erDiagram`) or Graphviz DOT, to a file or the clipboard.
  - The structure is read in a separate session, with progress.
- **Schema and data comparison** (`Compare with…` in the schema menu, or the database menu on engines without schemas). The source is compared with a schema on any connection, on the same engine or another server.
  - `Structure` tab: compares the objects themselves.
    - Tables are compared by columns (type, `NULL`, default, auto-increment, comment), primary key, indexes and foreign keys. Views, procedures, functions, triggers, sequences and types are compared by definition.
    - For each object you see what changed, its definition in the source and in the target, and the statements that sync it.
    - The script creates new objects (tables in foreign key order) and changes the changed ones with `ALTER TABLE`. In SQLite the table is rebuilt with its rows copied.
    - Dropping extra objects is optional and unticked by default.
    - Schema names in definitions are replaced with the target's.
  - `Data` tab: matches the rows of each table by primary key, between any two engines.
    - The target is read page by page and kept in memory as column hashes; the source streams past it.
    - The script is made of `DELETE`s (batched by key), `UPDATE`s of only the changed columns, and multi-row `INSERT`s. SQL Server gets `IDENTITY_INSERT` and PostgreSQL gets sequence moves.
    - Deletes run from children to parents, inserts the other way round.
    - In ClickHouse these become `ALTER TABLE … UPDATE / DELETE` mutations that wait for completion.
  - The script can be viewed, copied, saved to `.sql`, opened in a console on the target, or applied as a background job. It runs in one transaction where DDL is transactional (PostgreSQL, SQL Server and DuckDB; for data, every engine except ClickHouse) and rolls back on error.
  - Comparison runs in separate sessions, with progress and a `Stop` button.
- **Copying tables between connections** (`Copy to another connection…` in a schema or database menu, `Copy to…` in a table menu). The ticked tables are copied with their rows into a schema on any connection, also between different engines: all 36 pairs of the six.
  - Missing tables are created in foreign key order.
  - Types are translated for the target. For example, MySQL `tinyint(1)` becomes PostgreSQL `boolean`, `bytea` becomes SQL Server `varbinary(max)`, and a key becomes `ORDER BY` in ClickHouse.
  - The tab shows each column's type in the source and in the target before anything runs.
  - For tables the target already has, you can add the rows, replace them, or recreate the table. Replacing and recreating ask for confirmation.
  - Rows are read page by page by primary key and inserted in batches, one transaction per table (except on ClickHouse). SQL Server gets `IDENTITY_INSERT` and PostgreSQL gets sequence moves.
  - The copy runs as a background job with progress and `Cancel`.
- **Test data generator** (`Generate test data…` in a table menu).
  - Every column gets a generator picked from its name and type: sequences for keys, names, emails, phones, cities, companies, words, numbers and dates in a range, UUIDs, JSON and bytes.
  - MySQL and ClickHouse enum values are picked from the type, and foreign keys take values from the parent table.
  - Any generator and its settings can be changed. NULLs go into nullable columns at a chosen rate.
  - Values of primary keys and unique columns never repeat, also when you add more rows later. Sequences go on after the largest value already there.
  - A preview shows the first rows, and the same seed always makes the same rows.
  - The rows go in batches in one transaction (except on ClickHouse) as a background job, optionally after deleting the rows already there.
- **Structure and DDL** of any object: columns, indexes, foreign keys and the `CREATE` statement.
- **Table editor** (`New → Table`, `Modify Table…`).
  - Edits columns (type, `NOT NULL`, default, auto-increment and comment), the primary key, indexes and foreign keys.
  - A live SQL preview sits at the bottom, and the changes run in one transaction.
  - What it generates on each engine:
    - **PostgreSQL**: `ALTER TABLE` with `RENAME COLUMN`, `TYPE … USING` and identity.
    - **MySQL**: `CHANGE COLUMN`, recreating the affected foreign keys.
    - **SQL Server**: `ALTER COLUMN`, `sp_rename` and `IDENTITY`; default constraints are recreated, and comments go in extended properties.
    - **ClickHouse**: `MergeTree` with `ORDER BY` on the primary key, `MODIFY COLUMN`, `RENAME COLUMN`, data-skipping indexes and comments.
    - **SQLite**: the table is rebuilt through a temporary copy when `ALTER` is not enough.
    - **DuckDB**: `ALTER` as in PostgreSQL. What DuckDB can't do (auto-increment, changing keys of an existing table, foreign key actions), the editor writes in the script notes.
- **Creating and dropping** databases and schemas, and dropping, renaming and truncating (`TRUNCATE`) objects from the context menu.
  - The exact SQL is shown before it runs (`CASCADE` and `WITH (FORCE)` on PostgreSQL).
  - `CREATE` templates for views, functions, procedures, triggers and sequences open in a new console.
- **The window never waits for the database**: connections, queries, metadata loading, file dialogs, export and import all run in the background.
  - The console and the data view have a timer and a `Stop` button that cancels the query on the server:
    - PostgreSQL: `pg_cancel`;
    - MySQL: `KILL QUERY`;
    - SQL Server: the session is reopened, and an open transaction rolls back;
    - ClickHouse: `KILL QUERY` by `query_id`;
    - SQLite: `sqlite3_interrupt`;
    - DuckDB: `interrupt`.
  - The data view works in its own session, so a slow page does not hold up the object tree.
- **Search in a database** (`Search…` in a database or schema menu): finds a text in the names of tables, views, routines, triggers and other objects, in the definitions of views, routines and triggers (with the matching line), and, when `Data` is ticked, in the values of every table. Data search casts every column except binary ones to text and looks for the text in it, ignoring case (`ILIKE` on PostgreSQL and DuckDB, `positionCaseInsensitiveUTF8` on ClickHouse); `%` and `_` are plain characters. It counts the matching rows of each table, up to 1000, so it reads whole tables: on a large database, search one schema. The search runs on a session of its own, shows its progress and can be stopped. A click on a result opens the object, or for data the table with a `WHERE` filter that selects the matching rows.

![Search](crates/dbman/tests/snapshots/search.png)
- **Sessions** (`Sessions` in a server connection's menu): who is connected and what they run, from `pg_stat_activity` on PostgreSQL, `information_schema.PROCESSLIST` on MySQL and MariaDB, `sys.dm_exec_sessions` and `sys.dm_exec_requests` on SQL Server, and `system.processes` (running queries) on ClickHouse.
  - Each row has the user, database, client, application, state, how long the session has been in that state, what it waits for, and its current or last statement. The session DBMan lists them with is marked `this`.
  - The list reloads every 3 seconds (`Auto-refresh` turns that off), and `Hide idle` keeps only sessions that run something. The filter searches every column.
  - A click on a row shows the whole statement below the table. There, and in the row's menu, `Cancel query` stops the statement and keeps the session (`pg_cancel_backend`, `KILL QUERY`, ClickHouse `KILL QUERY WHERE query_id`), and `Kill session…` asks first and then disconnects it, rolling back its open transaction (`pg_terminate_backend`, `KILL CONNECTION`, SQL Server `KILL`). `Open query in a console` and `Copy query` take the statement.

![Sessions](crates/dbman/tests/snapshots/sessions.png)
- **Users and roles** (`Users and roles` in a server connection's menu, shown only when the connected user may manage users: a PostgreSQL superuser or `CREATEROLE` role, a MySQL account with `CREATE USER`, a SQL Server `sysadmin` or a holder of `ALTER ANY LOGIN` and `ALTER ANY USER`, a ClickHouse user with access management). SQLite and DuckDB have no users, so they have no such tab.
  - On the left, the users and roles (PostgreSQL roles that can log in are users; MySQL and MariaDB accounts are `user@host`; on SQL Server the users and roles of one database; ClickHouse users and roles). `Show built-in` adds `pg_*` roles, `mysql.sys`, `dbo`, fixed database roles and the like.
  - On the right, the picked one: its attributes (superuser, can create databases or roles, inherits, replication, row-level security bypass; locked MySQL account; disabled SQL Server login), the roles it belongs to and the members of a role (`×` removes one), and the privileges granted to it directly, with the object, the privilege and whether it may pass it on. On PostgreSQL and SQL Server the privileges are those of the database picked in the toolbar.
  - `New user…`, `New role…` and `Edit…` set the name (and host on MySQL), password and attributes, and on PostgreSQL the connection limit and until when the password is valid; `Add to role…` and `Add member…` grant a role, with the admin option where the engine has one; `Grant privileges…` grants privileges on a table, view, schema, database, sequence, function, every table of a schema or the whole server, depending on the engine, with the grant option, and picks the database, schema and object from lists; `Revoke…` in a privilege's menu takes it back, and `Drop…` drops the user or role (on PostgreSQL it can first hand the role's objects in the database to you with `REASSIGN OWNED` and `DROP OWNED`). Every editor shows the exact SQL (`CREATE ROLE`, `CREATE USER`, `ALTER ROLE`, `GRANT`, `REVOKE`, `ALTER ROLE … ADD MEMBER`…) before it runs. A new SQL Server user gets a login of the same name; dropping the user keeps the login.
  - On a read-only connection the tab only shows.

![Users and roles](crates/dbman/tests/snapshots/users_and_roles.png)
- **Backup and restore** from the context menu of a database, schema or table (`Back up…`, `Restore…`). The job runs in the background, and the window stays usable. The "Background tasks" panel shows progress, file size, time, the tool's log and a `Cancel` button.
  - **PostgreSQL**:
    - `pg_dump` in custom, plain SQL, directory (parallel) and tar formats;
    - choice of schemas and tables, skipping the data of chosen tables, `--no-owner` and `--no-privileges`;
    - for SQL, `DROP` / `CREATE DATABASE` and `INSERT`;
    - restore through `pg_restore` (format detected from the file; clean, create database, single transaction, parallel), or through `psql` for SQL scripts.
  - **MySQL / MariaDB**:
    - `mysqldump`, or `mariadb-dump` on MariaDB;
    - `--single-transaction`, routines, triggers, events, `DROP TABLE`, `CREATE DATABASE`, and hex for binary data;
    - restore with the `mysql` / `mariadb` client, with progress by how much of the file has been read.
  - **SQLite**:
    - a copy through the online backup API (the database stays available) or `VACUUM INTO`, checked with `quick_check`;
    - restore copies the pages back into the open database.
  - **SQL Server**:
    - `BACKUP DATABASE` / `BACKUP LOG`: full, differential or log, with copy-only, compression, checksums and a `RESTORE VERIFYONLY` check;
    - `RESTORE DATABASE` with replace, closing other sessions and `NORECOVERY`;
    - files can move to the server's default folders, to restore a copy under a new name;
    - the file lives on the server, progress comes from `sys.dm_exec_requests`, and `Cancel` ends the job with `KILL`.
  - **ClickHouse**:
    - `BACKUP` / `RESTORE` of a database or table to a server disk (`Disk('backups', …)`), a file (`File(…)`) or S3;
    - incremental copies on top of a previous one (`base_backup`), and structure only;
    - restore under another name, or by adding rows to existing tables;
    - the job runs asynchronously (`ASYNC`), progress and errors come from `system.backups`, and `Cancel` kills its query;
    - the S3 key is hidden in the log and preview;
    - the disk and path must be allowed in the server settings (example in `ci/clickhouse-backups.xml`).
  - **DuckDB**:
    - `EXPORT DATABASE` to a folder (Parquet with zstd, or CSV), or a copy to a new `.duckdb` file (`ATTACH` + `COPY FROM DATABASE`);
    - restore with `IMPORT DATABASE` from a folder, or by copying from a file;
    - no external tools are needed, and the database stays open during the copy. A DuckDB file can be opened only once per process, so all sessions on one file share one database instance.
  - Tools are looked up on `PATH` and in the usual install locations (Homebrew, Postgres.app, Program Files, `/usr/lib/postgresql/*/bin`). The folder can also be set in the dialog.
  - The password is passed through environment variables, so it never appears on the command line or in the log.
- **English (default) and Russian interface**. The language switch is in the top right corner and is remembered.
  - Translations live in `crates/dbman/locales/ru.json`, keyed by the English text.
  - A test checks that every interface text has a translation.
- Light and dark themes.

Shortcuts:

| Key | Action |
|---|---|
| `Ctrl+T` | New console |
| `Ctrl+W` | Close the tab |
| `Ctrl+Space` | Autocomplete in the console |
| `Ctrl+F` / `Ctrl+R` | Find / replace in the console |
| `Ctrl+S` | Save the console's script |
| `F4` | Open the selected object |
| Double click on a table | Open its data |

## Download

Every push to `main` builds the app for Linux (x86_64), Windows (x86_64) and macOS (Apple Silicon and Intel) in the [Release builds](../../actions/workflows/release.yml) workflow; the archives are in the run's artifacts. A tag like `v0.2.0` also publishes them as a [GitHub release](../../releases).

The macOS app is not notarized: open it the first time with right click → Open.

## Running from source

```sh
cargo run --release
```

The first build takes longer than usual because DuckDB compiles from C++ source (about 10 minutes on 4 cores). After that it comes from the cache.

On Linux the file dialog needs xdg-desktop-portal (present in regular GNOME and KDE).

## Architecture

```
crates/
  dbman-core/     models and the Connection trait, no GUI or drivers
  dbman-drivers/  implementations: postgres (tokio-postgres), mysql (mysql_async), sqlite (rusqlite), duckdb (duckdb-rs, built from source), mssql (tiberius), clickhouse (HTTP through reqwest)
  dbman/          the app on eframe + shadcn_egui
```

- **`dbman-core`**:
  - `ConnectionConfig` and `DbKind`;
  - the `Connection` trait: list databases, schemas and objects, table structure, DDL, and running SQL;
  - result models (`QueryResult`, `Cell`) and schema models (`DbObject`, `TableDetails`);
  - a SQL tokenizer for highlighting and for splitting a script into statements by dialect (PostgreSQL `$$` strings, MySQL backticks and `#` comments);
  - identifier and string quoting.
- **`dbman-drivers`**:
  - one module per engine, plus `connect(&ConnectionConfig)`;
  - `backup.rs` runs backups, and `transfer.rs` does data export and import. File formats and `INSERT` building live in `dbman-core::export`, `dbman-core::import` and `dbman-core::xlsx`; `files.rs` reads and writes Parquet files through DuckDB.
  - To add an engine, you need a `DbKind` variant, a `Connection` implementation and a line in `connect`.
- **`dbman`**: the interface never waits for the database. Queries run on a tokio runtime, and results come back as events through a channel that is drained every frame (`events.rs`). The modules:

  | Area | Module | Logic elsewhere |
  |---|---|---|
  | Object tree | `explorer.rs` | |
  | Tabs | `tabs.rs` | |
  | Result grid | `grid.rs` | |
  | Data editor | `data_view.rs` | |
  | Table editor | `table_editor.rs` | DDL in `dbman-core::ddl` |
  | Confirmation dialogs | `prompt.rs` | |
  | Running changes in a separate session | `exec.rs` | |
  | SQL editor | `sql_editor.rs` | |
  | Connection dialog | `connection_dialog.rs` | |
  | ER diagram | `er_view.rs` | model, layout and export in `dbman-core::er` |
  | Schema and data comparison | `compare_view.rs` | comparison and scripts in `dbman-core::compare`, reading and running in `dbman-drivers::compare` |
  | Copying between connections | `copy_view.rs` | type and value mapping in `dbman-core::copy`, the copy itself in `dbman-drivers::copy` |
  | Test data dialog | `generate_dialog.rs` | generators in `dbman-core::generate`, inserting in `dbman-drivers::generate` |
  | Target choice for comparison and copying | `target_picker.rs` | |
  | Interface language | `i18n.rs` | |
  | Backup dialogs | `backup_dialog.rs` | |
  | Data transfer dialogs | `transfer_dialog.rs` | |
  | Background jobs | `jobs.rs` | |
  | Server sessions | `sessions_view.rs` | statements per engine in `dbman-core::sessions` |
  | Users and roles | `admin_view.rs` | statements per engine in `dbman-core::admin` |

## Tests

```sh
cargo test --workspace
```

Driver tests for SQLite and DuckDB always run. PostgreSQL, MySQL, SQL Server, ClickHouse, CockroachDB and TiDB need server addresses:

```sh
DBMAN_TEST_POSTGRES=postgres://postgres:secret@127.0.0.1:5432/postgres \
DBMAN_TEST_MYSQL=mysql://root:secret@127.0.0.1:3306/dbman_test \
DBMAN_TEST_MSSQL=mssql://sa:Secret_1@127.0.0.1:1433/dbman_test \
DBMAN_TEST_CLICKHOUSE=clickhouse://default:secret@127.0.0.1:8123/dbman_test \
DBMAN_TEST_COCKROACH=postgres://root@127.0.0.1:26257/dbman_test \
DBMAN_TEST_TIDB=mysql://root@127.0.0.1:4000/dbman_test \
cargo test --workspace
```

`crates/dbman/tests/ui.rs` drives the whole window on a sample SQLite database.
- With `DBMAN_SCREENSHOTS=1 UPDATE_SNAPSHOTS=1` it redraws the screenshots in `crates/dbman/tests/snapshots/`.
- That needs a wgpu adapter. Without a GPU, `mesa-vulkan-drivers` works.

## Next

1. More engine features, with their backups:
   - **ClickHouse**: partitions and mutations in the object tree, user-defined functions, and the native protocol.
   - **SQL Server**: `.bacpac` export through `sqlpackage`, and Windows / Entra ID sign-in.

## License

MIT
