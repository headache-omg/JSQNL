# JSQNL v0.1 사용 안내서

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNL은 Rust로 작성한 로컬 임베디드 데이터베이스입니다. `jsqnl`로 쿼리를 실행하고 `in2jsqnl`로 데이터를 가져올 수 있습니다. Rust 라이브러리에서도 같은 엔진을 사용할 수 있습니다.

## 빌드와 실행

Rust 1.89 이상, Cargo, 시스템의 SQLite 개발 라이브러리가 필요합니다. 명령은 저장소의 루트에서 실행하세요.

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked
```

데이터를 수정하기 전에 예제 파일을 복사하세요. `exec`는 영구 데이터베이스를 조회하고 결과를 JSON Lines로 출력합니다. `run`은 혼합 스크립트를 메모리에서 실행합니다. `jsqnl init db.jsqnl`로 빈 데이터베이스를 만듭니다.

```sh
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl run examples/demo.jsqnl
```

## 파일 형식

`.jsqnl` 기본 파일에는 UTF-8 스키마 줄과 JSON 객체가 들어갑니다. 객체마다 물리적으로 한 줄을 사용하며 바로 앞에 선언된 스키마에 속합니다. 선택 헤더 `-- JSQNL: 1`은 첫 줄에 둡니다.

```sql
-- JSQNL: 1
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
```

지원하는 타입은 `INT`(부호 있는 64비트 정수), `FLOAT`, `TEXT`, `BOOL`, `JSON`, `VECTOR(n)`입니다. 벡터는 유한한 수를 정확히 `n`개 포함해야 합니다. 널 허용 필드는 생략하거나 `null`로 지정할 수 있습니다. 테이블마다 `INT` 또는 `TEXT` 기본 키를 하나 지정할 수 있습니다. 자세한 규칙은 [파일 형식 명세](../../FORMAT.md)를 참조하세요.

## 쿼리와 갱신

`WHERE`, `IN`, `BETWEEN`, `IS NULL`로 필터링하고 `ORDER BY`, `LIMIT`, `OFFSET`으로 정렬과 페이지 나누기를 수행합니다. `GROUP BY`와 `COUNT(*)`로 그룹별 행 수를 집계합니다. `INNER JOIN`과 `LEFT JOIN`은 같은 데이터베이스에 있는 두 테이블에서 각각 지정한 열 하나를 동등 조건으로 비교합니다.

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC LIMIT 5;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

`THE 1`과 `_line`은 테이블 내 논리 행 ID를 참조합니다. 물리적 줄 번호와 기본 키는 별개입니다. `WHO =`는 값을 바꾸고, `WHO +`는 숫자 덧셈, 문자열 결합 또는 JSON 배열 확장에 사용할 수 있습니다. `FUSE users THE 1 INTO THE 2 KEEP TARGET;`는 대상 행에서 생략되거나 `null`인 값을 채우고, 대상의 키와 ID를 보존한 뒤 원본 행을 삭제합니다.

```sql
RESONATE embedding FROM users WITH [1,0,0] LIMIT 5;
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`RESONATE`는 조건에 맞는 영벡터 이외의 벡터를 코사인 유사도로 정렬합니다. `DECAY`는 `FLOAT` 값 또는 벡터의 모든 성분에 0부터 1까지의 계수를 곱합니다.

## JSON·JSONL 가져오기와 Parquet 조회

IN2JSQNL은 지원되는 SQL `CREATE TABLE` 정의 하나와 JSON 객체, 객체 배열 또는 JSONL 데이터를 결합합니다. 필요하면 `--format json` 또는 `--format jsonl`로 형식을 지정하고, 여러 테이블 정의 중 하나는 `--table`로 선택합니다. 복합 키, 기본값, 외래 키는 지원 범위에 포함되지 않습니다.

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

`scan`은 동일한 스키마의 Parquet 분할 파일을 파일 이름 순서로 읽습니다. PyArrow가 설치되어 있어야 하며, 필요한 경우 `JSQNL_PYTHON`에 해당 Python 경로를 지정합니다. 테이블 이름은 디렉터리 이름을 사용합니다. 디렉터리가 `data`이면 상위 디렉터리 이름을 사용합니다. 한 번에 읽기 전용 `SELECT` 하나를 실행합니다.

```sh
./target/release/jsqnl scan /path/to/enwiki/data 'SELECT identifier FROM enwiki LIMIT 5;'
```

## 저장과 리소스 제한

갱신 로그가 없는 기본 파일에 디스크 오프셋 인덱스를 만들 수 있습니다. 이 인덱스는 `_line` 또는 기본 키의 동등 검색과 조건 없는 `COUNT(*)`를 지원합니다. `EXPLAIN ANALYZE`는 `SELECT`를 실행하고 접근 경로, 읽은 양, 실행 시간을 보고합니다. `check`는 데이터베이스를 검증하고 인덱스 상태를 보고합니다.

```sh
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
./target/release/jsqnl check users.jsqnl
./target/release/jsqnl fork users.jsqnl experiment
```

쓰기 배치가 성공하면 인접한 `.log` 파일에 커밋된 프레임 하나가 추가됩니다. 기본 파일과 로그를 함께 보관하세요. `fork`는 일관된 스냅샷을 새 디렉터리에 복제합니다. 스트리밍 도중 실패하면 일부 결과가 출력되었을 수 있으므로 종료 상태를 확인하세요.

대규모 조인, 그룹 집계, 정렬은 임시 디스크를 사용할 수 있습니다. `JSQNL_JOIN_MEMORY_BYTES`, `JSQNL_GROUP_MEMORY_BYTES`, `JSQNL_SORT_MEMORY_BYTES`로 작업별 메모리 예산을 조정하고, `JSQNL_TEMP_LIMIT_BYTES`로 쿼리당 임시 공간 한도를 지정합니다(기본값 8 GiB). 메모리 예산은 추정치입니다. 넓은 중간 행이나 인덱스 없는 기본 키 검증 시 더 많은 메모리를 사용할 수 있습니다. 갱신 로그가 있는 데이터베이스는 메모리에서 재구성합니다.

인증 로그를 사용하려면 `JSQNL_JOURNAL_KEY_FILE`에 보호된 32바이트 키 파일을 지정하세요. 버전 3은 커밋된 프레임을 인증하고 데이터베이스의 정규 절대 경로에 연결합니다. 사본에는 `fork`로 새 경로에 맞게 로그를 다시 인증할 수 있습니다. 커밋된 로그 끝부분이 유실되었는지 확인하려면 마지막 커밋 상태를 신뢰할 수 있는 별도 위치에 기록해야 합니다.

## Rust API

`jsqnl::execute`에 데이터베이스 경로, SQL, JSON 결과 행을 받는 콜백을 전달하면 `QueryReport`가 반환됩니다. 쓰기 배치는 결과 콜백을 호출하기 전에 커밋됩니다.

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{}", report.rows_output);
```

## 라이선스 및 기업 문의

JSQNL은 [MIT 라이선스](../../LICENSE)로 제공됩니다. 기업 및 상업적 이용에도 같은 라이선스 조건이 적용됩니다. 기업 도입 지원, 유지보수, 별도 계약은 유지관리자에게 문의하세요.
