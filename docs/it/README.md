# Guida a JSQNL v0.1

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNL è un database locale integrabile, scritto in Rust. Usa `jsqnl` per eseguire query, `in2jsqnl` per importare dati o la libreria Rust per incorporare il motore in un’applicazione.

## Compilazione e avvio

Installa Rust 1.89 o una versione successiva, Cargo e le librerie di sviluppo SQLite del sistema. Esegui i comandi dalla directory principale del repository.

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked
```

Copia l’esempio prima di modificarlo. `exec` interroga un database persistente e restituisce risultati in JSON Lines. `run` esegue uno script misto in memoria. Usa `jsqnl init db.jsqnl` per creare un database vuoto.

```sh
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl run examples/demo.jsqnl
```

## Formato dei file

Un file di base `.jsqnl` contiene definizioni di schema e oggetti JSON codificati in UTF-8. Ogni oggetto occupa una riga fisica e appartiene all’ultimo schema dichiarato. L’intestazione facoltativa `-- JSQNL: 1` va nella prima riga.

```sql
-- JSQNL: 1
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
```

I tipi supportati sono `INT` (intero con segno a 64 bit), `FLOAT`, `TEXT`, `BOOL`, `JSON` e `VECTOR(n)`. Un vettore deve contenere esattamente `n` numeri finiti. I campi nullable possono essere omessi o avere valore `null`. Ogni tabella può avere una chiave primaria di tipo `INT` o `TEXT`. Consulta la [specifica del formato](../../FORMAT.md).

## Query e aggiornamenti

Filtra con `WHERE`, `IN`, `BETWEEN` e `IS NULL`. Usa `ORDER BY`, `LIMIT` e `OFFSET` per ordinare e paginare e `GROUP BY` con `COUNT(*)` per contare le righe per gruppo. `INNER JOIN` e `LEFT JOIN` confrontano per uguaglianza una colonna qualificata di ciascuna delle due tabelle nello stesso database.

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC LIMIT 5;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

`THE 1` e `_line` fanno riferimento a un ID logico interno alla tabella, indipendente dal numero di riga fisica e dalla chiave primaria. `WHO =` sostituisce un valore. `WHO +` può sommare numeri, concatenare stringhe o estendere un array JSON. `FUSE users THE 1 INTO THE 2 KEEP TARGET;` completa i campi mancanti o nulli della riga di destinazione, ne conserva la chiave e l’ID e cancella la riga di origine.

```sql
RESONATE embedding FROM users WITH [1,0,0] LIMIT 5;
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`RESONATE` ordina i vettori non nulli selezionati in base alla similarità del coseno, escludendo i vettori zero. `DECAY` moltiplica un `FLOAT` o tutte le componenti di un vettore per un fattore compreso tra 0 e 1.

## Importare JSON e JSONL e interrogare Parquet

IN2JSQNL combina una definizione SQL `CREATE TABLE` supportata con un oggetto JSON, un array di oggetti o dati JSONL. Usa `--format json` o `--format jsonl` per indicare il formato e `--table` per scegliere una definizione nel file SQL. Chiavi composite, valori predefiniti e chiavi esterne non sono supportati.

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

`scan` legge partizioni Parquet con lo stesso schema in ordine di nome. È necessaria un’installazione di PyArrow. Se serve, imposta `JSQNL_PYTHON` con il percorso del relativo interprete. Il nome della tabella deriva dalla directory o dalla sua directory superiore se si chiama `data`. Ogni scansione accetta un solo `SELECT` in sola lettura.

```sh
./target/release/jsqnl scan /path/to/enwiki/data 'SELECT identifier FROM enwiki LIMIT 5;'
```

## Archiviazione e limiti operativi

Crea un indice di offset su disco per un file di base privo del log degli aggiornamenti. L’indice accelera le ricerche per uguaglianza su `_line` o sulla chiave primaria e il `COUNT(*)` senza filtri. `EXPLAIN ANALYZE` esegue una `SELECT` e riporta il metodo di accesso, la quantità di dati letti e il tempo. `check` verifica il database e riporta lo stato degli indici.

```sh
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
./target/release/jsqnl check users.jsqnl
./target/release/jsqnl fork users.jsqnl experiment
```

Ogni batch di scritture completato aggiunge una transazione al file `.log` adiacente. Conserva insieme il file di base e il log. `fork` copia un’istantanea coerente in una nuova directory. Una lettura in streaming può fallire dopo aver già prodotto alcune righe. Controlla lo stato di uscita prima di usare il risultato.

Join, raggruppamenti e ordinamenti di grandi dimensioni possono usare spazio temporaneo su disco. Configura i budget con `JSQNL_JOIN_MEMORY_BYTES`, `JSQNL_GROUP_MEMORY_BYTES` e `JSQNL_SORT_MEMORY_BYTES`. `JSQNL_TEMP_LIMIT_BYTES` imposta lo spazio per query (8 GiB per impostazione predefinita). I budget di memoria sono stime. Righe intermedie larghe o la verifica delle chiavi primarie senza indice possono aumentare i consumi. I database con un log di aggiornamento vengono ricostruiti in memoria.

Per autenticare il log, assegna a `JSQNL_JOURNAL_KEY_FILE` il percorso di un file protetto con una chiave di 32 byte. La versione 3 autentica le transazioni confermate e le associa al percorso canonico del database. `fork` autentica nuovamente la copia per il percorso di destinazione. Per rilevare la perdita delle ultime transazioni confermate serve una registrazione affidabile e separata dell’ultimo stato.

## Rust API

`jsqnl::execute` riceve il percorso, il codice SQL e una funzione di callback per le righe JSON. Restituisce un `QueryReport`. I batch di scrittura vengono confermati prima di invocare la callback dei risultati.

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{}", report.rows_output);
```

## Licenza e richieste aziendali

JSQNL è distribuito con [licenza MIT](../../LICENSE), le cui condizioni si applicano anche agli usi aziendali e commerciali. Contatta i responsabili del progetto per assistenza all’adozione, manutenzione o accordi separati.
