# Guide d’utilisation de JSQNL v0.1

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNL est une base de données locale et embarquée écrite en Rust. Utilisez `jsqnl` pour les requêtes, `in2jsqnl` pour les imports ou la bibliothèque Rust pour intégrer le moteur à une application.

## Compilation et démarrage

Installez Rust 1.89 ou ultérieur, Cargo et les bibliothèques de développement SQLite de votre système. Exécutez les commandes à la racine du dépôt.

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked
```

Copiez le fichier d’exemple avant de le modifier. `exec` interroge une base persistante et émet les résultats au format JSON Lines. `run` exécute un script mixte en mémoire. Pour créer une base vide, utilisez `jsqnl init db.jsqnl`.

```sh
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl run examples/demo.jsqnl
```

## Format des fichiers

Un fichier de base `.jsqnl` contient des schémas et des objets JSON encodés en UTF-8. Chaque objet occupe une ligne physique et appartient au dernier schéma déclaré. L’en-tête facultatif `-- JSQNL: 1` figure sur la première ligne.

```sql
-- JSQNL: 1
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
```

Les types pris en charge sont `INT` (entier signé sur 64 bits), `FLOAT`, `TEXT`, `BOOL`, `JSON` et `VECTOR(n)`. Un vecteur doit contenir exactement `n` nombres finis. Une colonne nullable accepte une valeur absente ou `null`. Une table peut avoir une clé primaire de type `INT` ou `TEXT`. Consultez la [spécification du format](../../FORMAT.md) pour les règles d’encodage et de compatibilité.

## Requêtes et modifications

Filtrez avec `WHERE`, `IN`, `BETWEEN` ou `IS NULL`. Utilisez `ORDER BY`, `LIMIT` et `OFFSET` pour trier et paginer, puis `GROUP BY` avec `COUNT(*)` pour compter les lignes par groupe. `INNER JOIN` et `LEFT JOIN` comparent une colonne qualifiée de chacune de deux tables de la même base.

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC LIMIT 5;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

`THE 1` et `_line` se réfèrent à un identifiant logique propre à la table, indépendant du numéro de ligne physique et de la clé primaire. `WHO =` remplace une valeur. `WHO +` peut additionner des nombres, concaténer du texte ou étendre un tableau JSON. `FUSE users THE 1 INTO THE 2 KEEP TARGET;` renseigne les champs absents ou nuls de la cible, conserve sa clé et son identifiant, puis supprime la source.

```sql
RESONATE embedding FROM users WITH [1,0,0] LIMIT 5;
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`RESONATE` classe les vecteurs non nuls retenus d’après leur similarité cosinus. `DECAY` multiplie un `FLOAT` ou chaque composante d’un vecteur par un facteur compris entre 0 et 1.

## Import de JSON ou JSONL et lecture de Parquet

IN2JSQNL combine une définition SQL `CREATE TABLE` prise en charge avec un objet JSON, un tableau d’objets ou du JSONL. Utilisez `--format json` ou `--format jsonl` pour préciser le format et `--table` pour choisir une définition dans un fichier SQL. Les clés composites, les valeurs par défaut et les clés étrangères ne sont pas prises en charge.

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

`scan` lit des fichiers Parquet de même schéma dans l’ordre de leurs noms. PyArrow doit être installé. Au besoin, indiquez l’interpréteur correspondant via `JSQNL_PYTHON`. Le nom de table est celui du répertoire, ou celui de son parent si le répertoire s’appelle `data`. Chaque lecture accepte un seul `SELECT` en lecture seule.

```sh
./target/release/jsqnl scan /path/to/enwiki/data 'SELECT identifier FROM enwiki LIMIT 5;'
```

## Stockage et limites d’exploitation

Créez un index de positions sur disque pour un fichier de base sans journal de mises à jour. L’index accélère les recherches par égalité sur `_line` ou la clé primaire et les `COUNT(*)` sans filtre. `EXPLAIN ANALYZE` exécute un `SELECT` et indique le mode d’accès, le volume lu et le temps d’exécution. `check` vérifie la base et indique l’état des index.

```sh
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
./target/release/jsqnl check users.jsqnl
./target/release/jsqnl fork users.jsqnl experiment
```

Chaque lot d’écritures validé ajoute une trame au journal `.log` adjacent. Conservez ensemble le fichier de base et le journal. `fork` copie un instantané cohérent dans un nouveau répertoire. Une lecture en continu peut échouer après avoir émis des lignes. Vérifiez le code de sortie avant d’utiliser le résultat.

Les jointures volumineuses, les regroupements et les tris peuvent écrire des données temporaires sur disque. Réglez les budgets avec `JSQNL_JOIN_MEMORY_BYTES`, `JSQNL_GROUP_MEMORY_BYTES` et `JSQNL_SORT_MEMORY_BYTES`, et le quota temporaire par requête avec `JSQNL_TEMP_LIMIT_BYTES` (8 Gio par défaut). Les budgets mémoire sont des estimations. Des lignes intermédiaires larges ou une validation de clés primaires sans index peuvent augmenter la consommation. Avec un journal de mises à jour, la base est reconstruite en mémoire.

Pour authentifier le journal, configurez `JSQNL_JOURNAL_KEY_FILE` avec un fichier protégé contenant une clé de 32 octets. La version 3 authentifie les trames validées et les lie au chemin canonique de la base. `fork` authentifie à nouveau la copie pour le chemin de destination. La détection de la perte des dernières trames exige un enregistrement séparé et fiable du dernier état validé.

## Rust API

`jsqnl::execute` reçoit le chemin, le SQL et une fonction de rappel pour les lignes JSON. La fonction renvoie un `QueryReport`. Les lots d’écritures sont validés avant l’appel de la fonction de rappel.

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{}", report.rows_output);
```

## Licence et demandes des entreprises

JSQNL est distribué sous [licence MIT](../../LICENSE), qui s’applique aussi aux usages professionnels et commerciaux. Pour une aide au déploiement, de la maintenance ou un contrat distinct, contactez l’équipe de maintenance.
