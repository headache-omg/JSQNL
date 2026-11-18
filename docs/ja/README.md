# JSQNL v0.1

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNLは、JSON Linesでデータを記述し、SQL風の文で検索・更新するローカルデータベースです。単独のRust CLIとして動作します。

## 実行する

Rust 1.89以降と、システムのSQLite開発ライブラリが必要です。

```sh
cargo build --release --locked
cargo test --locked

# SCHEMA・JSONL・SQLが混在するスクリプトをメモリ上で実行
./target/release/jsqnl run examples/demo.jsqnl

# サンプルをデータベースとして利用
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl exec users.jsqnl 'INJECT age FROM users THE 1 WHO + 2;'
./target/release/jsqnl exec users.jsqnl 'SELECT _line, age FROM users;'
./target/release/jsqnl index users.jsqnl
```

空のDBは `jsqnl init db.jsqnl` で作成できます。`jsqnl exec db.jsqnl` と `jsqnl exec db.jsqnl -` は標準入力からまとめて文を読みます。SELECTの出力はJSONLです。

## IN2JSQNLで既存データを取り込む

`in2jsqnl` は、既存SQLの `CREATE TABLE` とJSON・JSONLデータから形式1の `.jsqnl` を生成する独立したCLIです。

```sh
cargo run --locked --bin in2jsqnl -- \
  --schema examples/import.sql --input examples/import.jsonl --output imported.jsqnl
cargo run --locked -- exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

JSONはオブジェクト1件またはオブジェクトの配列、JSONLは1行につき1オブジェクトです。拡張子 `.jsonl` と `.ndjson` はJSONLとして読みます。形式の明示には `--format json` または `--format jsonl`、複数のテーブル定義からの選択には `--table 名前` を使います。1回の変換で1テーブルを出力します。データは行単位で処理します。JSON・JSONLの重複メンバーは入れ子も含めて拒否します。SQL定義は4 MiBかつ65,536トークンまで、JSONレコードは既定のレコードサイズと262,144値までです。

対応する定義は `CREATE TABLE` の基本型、`NOT NULL` と単一列の `PRIMARY KEY` です。`BIGINT` などの整数型はJSQNLの符号付き64ビット整数、`VARCHAR`・日付・UUIDは文字列、`JSONB` はJSONに変換します。文字列の宣言幅は取り込み時に検証します。生成後のTEXT列には幅制約がありません。複合主キー、外部キー、独立したUNIQUE、DEFAULT、CHECK、精度指定のDECIMAL、SQLダンプのデータ操作文などはエラーになります。日付やUUIDの値は文字列として取り込みます。変換後の操作にはJSQNLが対応するSQL文を使用できます。

出力先と同じディレクトリで検証済みデータを準備し、上書き禁止の原子的な名前変更で公開します。公開前に強制終了した場合は、出力先の隣に末尾が `.import` の一時ファイルが残ることがあります。

Rustからは `jsqnl::import` を呼び出せます。

## ファイル仕様とRust API

[FORMAT.md](../../FORMAT.md) は、`.jsqnl` の形式バージョン1、型、NULL、バージョンヘッダー、エスケープを定義します。ヘッダーのない既存ファイルもバージョン1として開けます。互換性テストはRust実装と独立したPython 3の読み取り実装で同じファイルを比較します。

Rustアプリからは `jsqnl::execute` にファイル、SQL、結果行を受け取るコールバックを渡せます。CLIも同じAPIを使用します。結果コールバックから同じDBを照会・更新できます。読み取り中に書き込んだ変更は、進行中のSELECTの結果には反映されません。

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{:?} rows={}", report.access, report.rows_output);
```

`init`、`run_script`、`scan_parquet`、`check`、`build_disk_index`、`fork` もライブラリから呼び出せます。

### 実行計画の計測

```sh
jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
```

`EXPLAIN ANALYZE` は1件のSELECTを実行し、結果行の代わりにアクセス方式、処理の順番、読んだ元ファイルとログのバイト数、読み取った元ファイルの行数、結果の件数、経過時間をJSONで報告します。Rust APIの `explain_analyze` にはSELECTを直接渡します。バイト数はパーサーに渡った論理バイト数で、OSの先読みや索引ファイルからのI/Oは含みません。逐次JOINで左側を読み直した分は含めます。索引によるCOUNTでは元ファイルの読み取りが0になります。

## Parquetアーカイブの直接検索

`jsqnl scan PARQUET_DIRECTORY 'SELECT ...;'` は、指定ディレクトリ直下の全Parquetファイルをファイル名順に読みます。対象テーブル名はディレクトリ名で、末尾が `data` の場合は親ディレクトリ名です。クエリは既存のJSQNLの `SELECT` を1文受け付け、WHEREの条件と列選択を順に適用します。必要列だけを読み、結果はJSONLで逐次出力します。対象は同一のArrowスキーマを持つシャードです。

Parquetの読み取りにはPyArrowが必要です。既定の `python3` で利用できるようにするか、`JSQNL_PYTHON` にインストール済みのPythonを指定してください。

```sh
jsqnl scan /path/to/archive/enwiki/data 'SELECT _line, identifier, name FROM enwiki WHERE identifier = 123;'
```

`_line` はディレクトリ全体の1始まりの行番号です。ファイルの追加・改名・並べ替えで変わり得ます。日付・日時はISO形式の文字列になります。Parquet側は読み取り専用です。スキャン中の読み出しエラーは非ゼロの終了コードで報告され、先に出力した行が残ることがあります。

PyArrow付きの環境では `cargo test --locked scan_queries_all_parquet_shards_in_filename_order -- --ignored` でParquet結合テストを実行できます。

## 言語仕様

```sql
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, tags JSON, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
SELECT _line, name FROM users WHERE age >= 18 AND (id = 2 OR id = 3);
INSERT INTO users (id, name) VALUES (3, 'Mika');
INSERT INTO users VALUES {"id":4,"name":"Hana","tags":["new"]};
UPDATE users SET age = 21 WHERE id = 3;
DELETE FROM users WHERE id = 4;
INJECT age FROM users THE 1 WHO + 2;
```

- `-- SCHEMA: table (...)` または `SCHEMA table (...)` を独立した1行に書きます。後続のJSONLレコードは直近のSCHEMAに属します。レコードはUTF-8のJSONオブジェクトを1物理行に1件ずつ記述します。空行と `--` コメントも使えます。
- 型は `INT`（符号付き64ビット整数）、`FLOAT`（JSON数値）、`TEXT`、`BOOL`、`JSON`、`VECTOR(n)` です。未宣言の列、重複する列宣言、空のスキーマを拒否します。各テーブルに `INT` または `TEXT` の `PRIMARY KEY` を1列まで指定できます。`NOT NULL` と主キーの省略・nullはエラーです。その他の列の省略はWHEREと明示的な列選択でnullとして評価されます。`SELECT *` は保存済みのフィールドを返します。
- SQL文はセミコロンで終了します。キーワードと型名の大文字・小文字は区別しません。識別子はASCIIの英字またはアンダースコアから始まり、英数字とアンダースコアを使用できます。識別子の大文字・小文字は区別します。文字列にはJSONの二重引用符、またはSQL式の単一引用符と `''` エスケープを使えます。
- INSERTは `VALUES (値, ...)` または列リストを省略した `VALUES {JSONオブジェクト}` を受け付けます。位置指定で列リストを省略するとスキーマの宣言順を使います。UPDATEの代入値とWHEREの比較右辺はリテラルです。
- WHEREは `=`、`!=`、`<>`、`<`、`<=`、`>`、`>=`、`AND`、`OR`、括弧に対応します。数値同士、文字列同士を大小比較できます。等値比較はJSON値に対して行い、`col = NULL` で欠損またはnullを検索できます。ANDがORより先に評価されます。WHEREを省略したUPDATE・DELETEは全件に作用します。SELECT結果は論理IDの昇順です。

### 検索件数と並べ替え

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC, name ASC LIMIT 10 OFFSET 5;
SELECT COUNT(*) AS total FROM users WHERE age IS NOT NULL;
```

`SELECT` は複数列の `ORDER BY`、`ASC` / `DESC`、`LIMIT`、`OFFSET` と `COUNT(*) [AS 名前]` に対応します。並べ替えの同点は論理ID順です。昇順ではnullが先で、降順では後です。WHEREには `IS NULL`、`IS NOT NULL`、`BETWEEN`、`IN` も書けます。nullの等値比較は従来のJSQNLに従います。同じ構文をParquetの `scan` にも使えます。並べ替えに `LIMIT` を付けると候補の保持数は `OFFSET + LIMIT` の約2倍までに抑えられます。並べ替えがメモリ予算を超えると、ソート済みの小さな一時ファイルに分け、最大8本ずつマージします。

### 結合とグループ集計

```sql
SELECT p.name, t.name FROM people AS p LEFT JOIN teams AS t ON p.team_id = t.id;
SELECT t.name, COUNT(*) AS total FROM people AS p JOIN teams AS t ON p.team_id = t.id GROUP BY t.name ORDER BY total DESC LIMIT 10;
SELECT category, COUNT(*) AS total FROM items GROUP BY category ORDER BY total DESC;
```

`JOIN`（`INNER JOIN`）と `LEFT [OUTER] JOIN` は、同じ `.jsqnl` 内の2テーブルを、ONで指定した左右1列の等値で結合します。ONにはテーブル名または `AS` で指定した別名を付けます。明示した修飾名はJSONの出力キーにも使われます。`SELECT *` の結合結果は修飾名付きです。LEFT JOINの未一致行では右側がnullになります。null同士は結合条件に一致しません。ログのないファイルへの単独のJOINでは、ファイルが既定の16 MiBを超えるとGrace Hash Joinを使います。必要列だけを左右64組の一時パーティションに分け、右側を1パーティションずつメモリに載せて照合します。設定した予算を超えるパーティションは一時SQLite索引で照合するため、同一キーへの偏りがあっても右側を丸ごとメモリに載せません。予算は `JSQNL_JOIN_MEMORY_BYTES`（最小1,024バイト）で変更できます。バイト予算は直列化サイズの目安で、RSSの上限ではありません。

一時ファイルは `TMPDIR` に置き、通常の終了やエラー時に削除します。強制終了後の残留ファイルは次のCLI起動時、または次回の一時領域確保時に回収します。出力順が必要な場合は `ORDER BY` を指定してください。`EXPLAIN ANALYZE` は `grace_hash_join`、一時ファイルのサイズ合計、予算を超えたパーティション数を表示します。小さいファイルは従来のメモリ結合を使い、更新ログのあるDBは再構成して結合します。

`GROUP BY` は1列以上を指定でき、`COUNT(*)` でグループ内の行数を集計できます。SELECTに書く列はGROUP BYに含めます。NULLと欠損値は同じグループになります。ORDER BYにはグループ列またはCOUNTの別名を指定できます。ログのない単一テーブルの集計は入力を順次処理し、メモリにはグループごとの状態を保持します。集計メモリはグループ数に依存します。Parquetの `scan` でも単一テーブルのグループ集計が使えます。

### THE と WHO

`INJECT column FROM table THE n WHO operator value;` は指定行の1列を更新します。

- `THE n` はテーブルごとに1から割り当てる永続的な論理IDです。WHEREと列選択では `_line` で参照できます。元ファイルの物理行番号や主キーとは独立しており、削除済みIDは再利用しません。`_line` は予約列で、更新や挿入の列指定には使えません。
- `WHO =` は値を置換します。`+`、`-`、`*`、`/` は数値演算で、`+` は文字列連結にも使えます。JSON列の配列に `+` で配列を連結したり、単一のJSON値を末尾に追加したりできます。ゼロ除算、INTの非整数除算、結果の範囲超過、対象行の不在はエラーです。
- `VECTOR(n)` は正の固定次元で、値を指定する場合は有限のJSON数値をちょうどn個持つ配列が必要です。全置換は同じ検証を通ります。WHOによるベクトル演算と次元変更はエラーです。

## 保存形式と復旧

`exec` のDBファイルにはSCHEMAとJSONLレコードを置きます。SQLを含む配布用サンプルや一時分析には `run` を使います。空DBへの初回投入や、後から追加するスキーマとJSONLレコードは `exec` の入力にも書けます。

- `db.jsqnl` と `db.jsqnl.log` が永続化されたデータです。変更がある呼び出しごとに、挿入・更新後の行、削除、スキーマ追加を1つのチェックサム付きフレームとしてログに追記します。更新ログのあるDBでは元ファイルとログを読み、メモリ上で再構成します。
- 1回の `exec` を1バッチとして扱います。解析、実行、検証がすべて成功してから保存します。同じローカルファイルを開くCLIはファイルロックで直列化されます。改行まで到達していない末尾フレームは無視し、次回保存時に取り除きます。改行で完結したフレームの破損、ログの順序不一致、保存開始後の元ファイルの変更はエラーになります。
- `db.jsqnl.idx` は論理IDから最新の保存位置を引く再生成可能なインデックスです。元ファイルのレコードはJSONオブジェクトの開始位置、ログのレコードはフレームの開始位置とフレーム内の変更番号を記録します。オフセットと変更番号は0始まりです。書き込み後に更新され、`jsqnl index db.jsqnl` でも再生成できます。このJSON索引は保存位置の確認に使います。等値検索用のディスク索引は下記の `disk-index` で作成します。実行時のTHEは論理IDの順序付きマップを参照します。
- DBを移動・バックアップする際は書き込み中のCLIを終了させて、元ファイルとログをセットで扱ってください。実行中のファイルの置換、ハードリンク経由のアクセス、ネットワークファイルシステムはサポート対象外です。ログの圧縮は次段階です。保存完了直後にプロセスが停止した場合や、保存後に標準出力の書き込みに失敗した場合は、再実行前にSELECTで保存結果を確認してください。

更新ログが空または存在しないDBでは、`exec` に渡された単独の `SELECT` は元ファイルを1行ずつ読みます。元ファイル全体と記事本文をメモリに保持しません。有効なディスク索引があれば構築時の主キー検証を再利用します。索引がなければ重複検証のためにキーを保持します。結果は逐次出力されるため、後続行の破損などで失敗した場合に先行結果が残ることがあります。単独のSELECT以外のクエリバッチ、更新ログがあるDB、`index` はメモリ上での再構成を使います。ログのないDBの `check` と `fork` は行を逐次検証します。既存のJSON診断用索引があれば、`check` は照合のためその索引をメモリに読み込みます。

テーブルをメモリに再構成した後は、WHEREの `_line = 値` または主キーの等値条件を対象行の索引検索に使います。ANDで情報を絞る場合も、候補行に対してWHERE全体を評価します。

### リソース上限と認証付きログ

| 設定 | 既定 | 対象 |
| --- | ---: | --- |
| `JSQNL_SORT_MEMORY_BYTES` | 16 MiB | 並べ替え用メモリの見積もり。超過時はディスクへ退避 |
| `JSQNL_RECORD_LIMIT_BYTES` | 8 MiB | JSQNLの物理行と、書き込むレコードのサイズ |
| `JSQNL_FRAME_LIMIT_BYTES` | 64 MiB | 1つの更新ログフレームと、読み込むソート用退避レコード |
| `JSQNL_MATERIALIZED_LIMIT_BYTES` | 128 MiB | 全件再構成する経路の入力バイト数とメモリ使用量の見積もり |
| `JSQNL_TEMP_LIMIT_BYTES` | 8 GiB | クエリ単位の一時領域。JOIN・GROUP BY・ORDER BYで共有 |

SQLと混在スクリプトの入力は64 MiBかつ65,536トークンまでです。1つのレコードに含められるJSON値は262,144個までです。入力サイズや一時領域には上限があります。演算子のメモリ予算は見積もり値です。索引のないDBでの主キー検証や、幅の広い結果行・集計キーの構築では、使用メモリが増える場合があります。ログ付きの巨大DBでも、現行の更新と再生は全件再構成が必要です。コミット前に、確定済みログと新しいフレームを含めて再構成時の入力上限を確認します。

認証付きログを使うには、所有者だけがアクセスできる場所に32バイトの乱数鍵を生成し、そのパスを `JSQNL_JOURNAL_KEY_FILE` に指定します。この設定で新規ログはHMAC-SHA-256付きのバージョン3になります。認証はDBの正規化した絶対パスに結び付きます。同じ鍵でも別のパスにログをコピーすると検証に失敗します。移動や複製には、移動先に合わせてログを再認証する `jsqnl fork` を使います。バージョン1のログは鍵なしで開けます。以前のバージョン2は、信頼済みのログに限り `JSQNL_ALLOW_LEGACY_AUTH=1` で読み取り専用として開けます。この設定と同じ鍵で `fork` すると、分岐先をバージョン3に移行できます。

HMACはログに含まれる確定フレームの内容を検証します。ログ全体の削除、終端の切り詰め、古いスナップショットへの巻き戻しには、別途、信頼できる場所に記録した最終状態が必要です。鍵はDBディレクトリへの書き込み権限と分離して管理してください。

### ディスク上のバイトオフセット索引

```sh
jsqnl disk-index db.jsqnl
jsqnl exec db.jsqnl 'SELECT * FROM users WHERE id = 2;'
```

`disk-index` は更新ログが空または存在しないDBを逐次検証し、テーブル・論理ID・主キーからバイトオフセットを引けるSQLiteファイル `db.jsqnl.seek.sqlite` を作ります。単独のSELECTで `_line` または主キーを等値指定すると、ファイルの状態が索引作成時と一致する場合に該当行へ直接移動します。ANDで併記した条件に対しても、取得した行のWHERE全体を評価します。WHEREなしの `COUNT(*)` も、データ行を開かず索引から求めます。バッファは256 KiBで行を逐次読み込み、索引構築はSQLiteのページキャッシュを64 MiBに設定します。

新しい索引には `.seek.sqlite.seal` が付きます。通常のクエリはベースと索引ファイルの指紋を照合します。索引が変更された場合、シールがない旧索引の場合、元ファイルを変更・移動した場合は、逐次走査に戻ります。`check` はログのないDBについて索引のSHA-256まで読み取って確認し、`disk_index` に状態を出力します。`disk-index` で索引とシールを再生成できます。シールは破損検出用で、外部から渡された索引の認証には使えません。更新ログがあるDBは従来どおり更新を再生します。`index` が出力するJSONの診断用索引も利用できます。

### ファイルの取り扱い

DBのベースファイルとログ、索引には通常ファイルを使います。Unixではハードリンクも拒否します。ログと索引のパスにあるシンボリックリンクは開きません。Parquetの分割も通常ファイルが必要です。SQLiteのディスク索引はJSQNLが生成したものを使い、SQLiteの補助ジャーナルがある場合は拒否します。外部からDBを取り込む際は、同梱された索引を使わず、信頼できる保存先で再作成してください。バージョン1のチェックサムは破損検出用です。書き手の認証が必要な場合はバージョン3を使います。DBディレクトリは信頼できる利用者にだけ書き込みを許可してください。

### 整合性の確認

`jsqnl check db.jsqnl` は元ファイルとログを読み直し、確定済みログのバイト数、書きかけの末尾のバイト数、テーブルごとの有効行数をJSONで出力します。索引ファイルは `current`（一致）、`missing`（未作成）、`stale`（不一致）、`invalid`（JSONとして読み取れない）のいずれかで報告します。確定ログや元ファイルに異常があれば失敗します。診断はファイルを書き換えません。索引の再生成には `jsqnl index db.jsqnl` を使います。

## ベクトル類似検索

```sql
RESONATE embedding FROM users WITH [1,0,0] WHERE age >= 18 LIMIT 5;
```

`RESONATE column FROM table WITH [数値, ...] [WHERE 条件] LIMIT 件数;` は、条件に一致する行からコサイン類似度の高い順に最大指定件数を返します。出力は `_line` と `_score` を持つJSONLです。同点は論理IDの昇順になります。列は `VECTOR(n)`、検索ベクトルは同じ次元の非ゼロベクトルが必要です。欠損、null、ゼロベクトルの行は検索結果から除外します。検索は対象行を走査し、DBを更新しません。

## 外部ハッシュ集計と一時領域

GROUP BYは集計キーごとの状態をHashMapに保持し、推定メモリが既定の16 MiBを超えると256個のディスクパーティションに退避します。1パーティションずつ集計し、偏りでメモリ予算を超えるパーティションは一時SQLite集計に切り替えます。グループ数が多い場合も、ORDER BYなしなら結果を逐次出力します。ORDER BYはメモリ予算を超えると外部ソートを使います。LIMITを指定すると、必要な上位候補だけを残せます。集計の予算は `JSQNL_GROUP_MEMORY_BYTES` で変更できます。予算は推定ヒープ使用量で、プロセスのRSS上限ではありません。

集計とGrace Hash Joinの一時データは、`TMPDIR` 内の利用者専用レジストリにクエリ単位で保存されます。CLI起動時および一時領域の確保時に、OSロックが解放されたディレクトリだけを回収します。Rustからは `jsqnl::recover_temporary_files()` を呼べます。クエリごとの一時容量は `JSQNL_TEMP_LIMIT_BYTES` で指定でき、既定は8 GiBです。集計とJOINを同時に行うクエリでは同じ予算を共有します。複数クエリの合計容量は各クエリの上限の合計まで増え得ます。

## 減衰

```sql
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`DECAY column FROM table BY 係数 [WHERE 条件];` は、`FLOAT` の値または `VECTOR(n)` の全要素に、0以上1以下の係数を掛けます。WHEREの省略時は全行が対象です。欠損値とnullはそのまま保持し、係数1では更新を省略します。演算は64ビット浮動小数点で行い、変更は他の更新と同じバッチに記録されます。ベクトルの次元は維持されます。

## 行の統合とデータベースの分岐

```sql
FUSE users THE 1 INTO THE 2 KEEP TARGET;
```

`FUSE table THE 元の論理ID INTO THE 統合先の論理ID [KEEP TARGET];` は、統合先で欠損またはnullの列を元の行の値で補います。統合先の主キーと論理IDを維持し、元の行を同じバッチで削除します。主キー以外の列に異なる非null値がある場合はエラーになります。`KEEP TARGET` を指定すると、競合した列では統合先の値を採用します。ベクトルやJSON値は列全体を単位に扱います。

```sh
jsqnl fork users.jsqnl experiment
jsqnl exec experiment/users.jsqnl 'SELECT _line, name FROM users;'
```

`fork` は指定した新規ディレクトリに、元のファイルと確定済みログを複製します。主キー、論理ID、削除済みIDの履歴は引き継がれます。分岐元と分岐先はそれぞれ独立して更新でき、同じ名前の既存ディレクトリがある場合はエラーになります。分岐先の索引は `index` または最初の変更保存時に作成されます。

## ライセンスと法人向けのご相談

JSQNLは[MITライセンス](../../LICENSE)で提供しています。法人の利用や商用利用にも、MITライセンスの条件が適用されます。

法人向けの導入支援・保守・個別契約をご希望の場合は、開発元までご相談ください。
