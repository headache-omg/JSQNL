# Guia de utilização do JSQNL v0.1

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNL é uma base de dados local e incorporável escrita em Rust. Use `jsqnl` para consultas, `in2jsqnl` para importações ou a biblioteca Rust para integrar o motor numa aplicação.

## Compilação e primeiros passos

Instale Rust 1.89 ou posterior, Cargo e as bibliotecas de desenvolvimento do SQLite do seu sistema. Execute os comandos na raiz do repositório.

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked
```

Copie o ficheiro de exemplo antes de o modificar. `exec` consulta uma base de dados persistente e apresenta os resultados em JSON Lines. `run` executa um script misto em memória. Para criar uma base de dados vazia, use `jsqnl init db.jsqnl`.

```sh
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl run examples/demo.jsqnl
```

## Formato dos ficheiros

Um ficheiro base `.jsqnl` contém esquemas e objetos JSON codificados em UTF-8. Cada objeto ocupa uma linha física e pertence ao último esquema declarado. O cabeçalho opcional `-- JSQNL: 1` fica na primeira linha.

```sql
-- JSQNL: 1
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
```

Os tipos disponíveis são `INT` (inteiro com sinal de 64 bits), `FLOAT`, `TEXT`, `BOOL`, `JSON` e `VECTOR(n)`. Um vetor deve conter exatamente `n` números finitos. Os campos que aceitam nulos podem ser omitidos ou definidos como `null`. Cada tabela pode ter uma chave primária `INT` ou `TEXT`. Consulte a [especificação do formato](../../FORMAT.md).

## Consultas e atualizações

Filtre com `WHERE`, `IN`, `BETWEEN` ou `IS NULL`. Use `ORDER BY`, `LIMIT` e `OFFSET` para ordenar e paginar, e `GROUP BY` com `COUNT(*)` para contar linhas por grupo. `INNER JOIN` e `LEFT JOIN` comparam por igualdade uma coluna qualificada de cada uma de duas tabelas da mesma base.

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC LIMIT 5;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

`THE 1` e `_line` referem-se ao identificador lógico da linha dentro da tabela. Este é independente da linha física e da chave primária. `WHO =` substitui um valor. `WHO +` soma números, concatena texto ou acrescenta elementos a um array JSON. `FUSE users THE 1 INTO THE 2 KEEP TARGET;` preenche os campos ausentes ou nulos da linha de destino, conserva a sua chave e o seu identificador e elimina a linha de origem.

```sql
RESONATE embedding FROM users WITH [1,0,0] LIMIT 5;
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`RESONATE` ordena os vetores não zero selecionados pela similaridade do cosseno. `DECAY` multiplica um `FLOAT` ou cada componente de um vetor por um fator entre 0 e 1.

## Importar JSON ou JSONL e consultar Parquet

IN2JSQNL combina uma definição SQL `CREATE TABLE` compatível com um objeto JSON, um array de objetos ou dados JSONL. Use `--format json` ou `--format jsonl` para indicar o formato e `--table` para escolher uma definição no ficheiro SQL. Chaves compostas, valores predefinidos e chaves estrangeiras ficam fora do subconjunto compatível.

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

`scan` lê ficheiros Parquet de esquema idêntico pela ordem dos respetivos nomes. É necessário ter o PyArrow instalado. Se necessário, indique o interpretador correspondente em `JSQNL_PYTHON`. O nome da tabela é o da pasta ou, se esta se chamar `data`, o da pasta superior. Cada leitura aceita um único `SELECT`.

```sh
./target/release/jsqnl scan /path/to/enwiki/data 'SELECT identifier FROM enwiki LIMIT 5;'
```

## Armazenamento e limites de operação

Crie um índice de posições em disco para um ficheiro base sem registo de atualizações. O índice acelera pesquisas por igualdade em `_line` ou na chave primária e `COUNT(*)` sem filtros. `EXPLAIN ANALYZE` executa um `SELECT` e indica o modo de acesso, os dados lidos e o tempo de execução. `check` verifica a base de dados e indica o estado dos índices.

```sh
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
./target/release/jsqnl check users.jsqnl
./target/release/jsqnl fork users.jsqnl experiment
```

Cada lote de escrita confirmado acrescenta uma transação ao ficheiro `.log` adjacente. Guarde o ficheiro base e o registo em conjunto. `fork` copia um instantâneo consistente para uma nova pasta. Uma leitura em fluxo pode falhar depois de já ter emitido algumas linhas. Verifique o código de saída antes de usar os resultados.

Junções, agrupamentos e ordenações de grande dimensão podem recorrer a espaço temporário em disco. Ajuste os orçamentos com `JSQNL_JOIN_MEMORY_BYTES`, `JSQNL_GROUP_MEMORY_BYTES` e `JSQNL_SORT_MEMORY_BYTES`. `JSQNL_TEMP_LIMIT_BYTES` define o espaço temporário por consulta (8 GiB por omissão). Os orçamentos de memória são estimativas. Linhas intermédias largas ou a verificação de chaves primárias sem um índice válido podem aumentar o consumo. Uma base com registo de atualizações é reconstruída em memória.

Para autenticar o registo, defina `JSQNL_JOURNAL_KEY_FILE` como o caminho para um ficheiro protegido com uma chave de 32 bytes. A versão 3 autentica as transações confirmadas e associa-as ao caminho canónico da base. Use `fork` para voltar a autenticar a cópia para o novo caminho. A deteção da perda das últimas transações confirmadas requer um registo separado e fidedigno do último estado.

## Rust API

`jsqnl::execute` recebe o caminho, o SQL e uma função de retorno de chamada para as linhas JSON. Devolve um `QueryReport`. Os lotes de escrita são confirmados antes de chamar a função de resultados.

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{}", report.rows_output);
```

## Licença e pedidos empresariais

O JSQNL é distribuído sob a [Licença MIT](../../LICENSE). Os seus termos também se aplicam à utilização empresarial e comercial. Contacte os responsáveis pelo projeto para apoio à implementação, manutenção ou acordos separados.
