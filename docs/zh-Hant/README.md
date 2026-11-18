# JSQNL v0.1 使用指南

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNL 是以 Rust 撰寫的本機嵌入式資料庫。`jsqnl` 用於查詢，`in2jsqnl` 用於匯入資料，也可透過 Rust 程式庫呼叫查詢引擎。

## 建置與執行

請安裝 Rust 1.89 以上版本、Cargo，以及系統的 SQLite 開發程式庫。請在儲存庫根目錄執行下列指令。

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked
```

更新資料前請先複製範例。`exec` 查詢持久化資料庫，並以 JSON Lines 輸出結果。`run` 在記憶體中執行混合腳本。可用 `jsqnl init db.jsqnl` 建立空資料庫。

```sh
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl run examples/demo.jsqnl
```

## 檔案格式

`.jsqnl` 基礎檔由 UTF-8 編碼的結構定義與 JSON 物件組成。每個物件各佔一個實體行，隸屬於最近出現的結構定義。選用的 `-- JSQNL: 1` 標頭位於第一行。

```sql
-- JSQNL: 1
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
```

支援 `INT`（64 位元有號整數）、`FLOAT`、`TEXT`、`BOOL`、`JSON` 和 `VECTOR(n)`。向量必須包含剛好 `n` 個有限數值。允許為空的欄位可以省略或填入 `null`。每個資料表可設置一個 `INT` 或 `TEXT` 主鍵。請參閱[檔案格式規格](../../FORMAT.md)。

## 查詢與更新

可用 `WHERE`、`IN`、`BETWEEN`、`IS NULL` 篩選資料，使用 `ORDER BY`、`LIMIT`、`OFFSET` 排序與分頁，以及用 `GROUP BY` 搭配 `COUNT(*)` 計數。`INNER JOIN` 與 `LEFT JOIN` 以同一資料庫中的兩個資料表各自指定的一個欄位進行等值連接。

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC LIMIT 5;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

`THE 1` 和 `_line` 都以資料表內的邏輯列 ID 為基礎，與實體行號及主鍵無關。`WHO =` 取代值，`WHO +` 可加總數值、串接字串或擴充 JSON 陣列。`FUSE users THE 1 INTO THE 2 KEEP TARGET;` 補齊目的列省略或為 `null` 的欄位，保留目的列的主鍵和 ID，並刪除來源列。

```sql
RESONATE embedding FROM users WITH [1,0,0] LIMIT 5;
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`RESONATE` 依餘弦相似度排序符合條件的非零向量。`DECAY` 將 `FLOAT` 或向量的每個元素乘上 0 到 1 之間的係數。

## 匯入 JSON、JSONL 與查詢 Parquet

IN2JSQNL 結合一個受支援的 SQL `CREATE TABLE` 定義和 JSON 物件、JSON 物件陣列或 JSONL。可用 `--format json` 或 `--format jsonl` 指定格式，並用 `--table` 選擇 SQL 檔中的資料表。複合主鍵、預設值和外鍵不在支援範圍內。

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

`scan` 依檔名順序讀取結構一致的 Parquet 分檔。請先安裝 PyArrow，必要時以 `JSQNL_PYTHON` 指定對應的 Python。資料表名稱取自目錄名稱。若目錄名為 `data`，則採用上層目錄名稱。每次掃描可執行一個唯讀 `SELECT`。

```sh
./target/release/jsqnl scan /path/to/enwiki/data 'SELECT identifier FROM enwiki LIMIT 5;'
```

## 儲存與資源限制

可為沒有更新日誌的基礎檔建立磁碟位移索引，加速 `_line` 或主鍵的等值查詢，以及不帶條件的 `COUNT(*)`。`EXPLAIN ANALYZE` 執行 `SELECT` 並回報存取方式、讀取量和執行時間。`check` 驗證資料庫並回報索引狀態。

```sh
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
./target/release/jsqnl check users.jsqnl
./target/release/jsqnl fork users.jsqnl experiment
```

成功的寫入批次會在相鄰的 `.log` 檔追加一筆已提交的紀錄。請一併保存基礎檔和日誌。`fork` 將一致的快照複製到新目錄。串流讀取失敗時可能已輸出部分資料，使用前請確認結束狀態。

大型連接、分組和排序可使用暫存磁碟。可用 `JSQNL_JOIN_MEMORY_BYTES`、`JSQNL_GROUP_MEMORY_BYTES`、`JSQNL_SORT_MEMORY_BYTES` 調整記憶體預算，以 `JSQNL_TEMP_LIMIT_BYTES` 設定每個查詢的暫存容量（預設 8 GiB）。記憶體預算是估算值。寬列或缺少索引時的主鍵驗證可能提高記憶體用量。有更新日誌的資料庫會在記憶體中重建。

使用驗證日誌時，將 `JSQNL_JOURNAL_KEY_FILE` 指向受保護的 32 位元組金鑰檔。第 3 版驗證已提交的紀錄，並與資料庫的正規絕對路徑綁定。`fork` 會針對目的路徑重新驗證並簽署日誌。若要偵測已提交的尾端紀錄遺失，須將最新提交狀態另存於可信位置。

## Rust API

`jsqnl::execute` 接受資料庫路徑、SQL，以及接收 JSON 結果列的回呼函式，並傳回 `QueryReport`。寫入批次會先提交，再呼叫結果回呼函式。

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{}", report.rows_output);
```

## 授權與企業洽詢

JSQNL 採用 [MIT 授權](../../LICENSE)。企業及商業用途同樣適用該授權條款。如需企業導入、維護或個別合約，請聯絡維護者。
