"""Independent JSQNL format-v1 compatibility reader using Python's standard library."""
import json
import math
import re
import sys

IDENTIFIER = r"[A-Za-z_][A-Za-z0-9_]*"
TYPES = dict(INTEGER="INT", NUMBER="FLOAT", REAL="FLOAT", STRING="TEXT", BOOLEAN="BOOL")
COLUMN = re.compile(
    rf"({IDENTIFIER})\s+(INT|INTEGER|FLOAT|NUMBER|REAL|TEXT|STRING|BOOL|BOOLEAN|JSON|VECTOR\s*\(\s*[0-9]+\s*\))"
    r"((?:\s+(?:NOT\s+NULL|PRIMARY\s+KEY))*)", re.IGNORECASE,
)


def reject(message):
    raise ValueError(message)


def finite_number(text):
    value = float(text)
    if not math.isfinite(value):
        reject("non-finite number")
    return value


def is_number(value):
    return type(value) in (int, float)


def valid(value, kind, required, dimension):
    if value is None:
        return not required
    if kind == "INT":
        return type(value) is int and -(2**63) <= value < 2**63
    if kind == "FLOAT":
        return is_number(value)
    if kind == "VECTOR":
        return isinstance(value, list) and len(value) == dimension and all(map(is_number, value))
    if kind == "TEXT":
        return isinstance(value, str)
    if kind == "BOOL":
        return type(value) is bool
    return kind == "JSON"


def read_schema(text):
    match = re.fullmatch(rf"({IDENTIFIER})\s*\((.*)\)\s*;?\s*(?:--.*)?", text)
    if not match:
        reject("invalid SCHEMA")
    name, raw_columns = match.groups()
    primary = None
    columns = {}
    for raw in raw_columns.split(","):
        match = COLUMN.fullmatch(raw.strip())
        if not match:
            reject("invalid column")
        field, kind, constraints = match.groups()
        if field == "_line" or field in columns:
            reject("reserved or duplicate column")
        kind = kind.upper()
        dimension = None
        if kind.startswith("VECTOR"):
            dimension = int(re.search(r"[0-9]+", kind).group())
            if dimension == 0:
                reject("invalid VECTOR dimension")
            kind = "VECTOR"
        kind = TYPES.get(kind, kind)
        constraints = " ".join(constraints.upper().split())
        if constraints.count("PRIMARY KEY") > 1 or constraints.count("NOT NULL") > 1:
            reject("duplicate constraint")
        if "PRIMARY KEY" in constraints:
            if primary is not None or kind not in {"INT", "TEXT"}:
                reject("invalid primary key")
            primary = field
        columns[field] = (kind, bool(constraints), dimension)
    return name, columns, primary


def read(path):
    schemas = {}
    tables = {}
    current = None
    with open(path, encoding="utf-8-sig", newline="") as source:
        for number, line in enumerate(source, 1):
            text = line.strip()
            header = re.fullmatch(r"--\s*JSQNL:\s*(.*)", text, re.IGNORECASE)
            if header:
                if number != 1 or header[1] != "1":
                    reject("unsupported version or position")
                continue
            schema = re.match(r"(?:--\s*SCHEMA:\s*|SCHEMA\s+)(.*)", text, re.IGNORECASE)
            if schema:
                current, columns, primary = read_schema(schema[1])
                if current in schemas:
                    reject("duplicate table")
                schemas[current] = columns, primary, set()
                tables[current] = []
                continue
            if not text or text.startswith("--"):
                continue
            if current is None:
                reject("JSONL row before SCHEMA")
            if "\r" in line.rstrip("\r\n"):
                reject("invalid physical line")
            row = json.loads(text, parse_constant=reject, parse_float=finite_number,
                             parse_int=lambda n: -0.0 if n == "-0" else int(n))
            if not isinstance(row, dict):
                reject("expected a JSON object")
            # Disallow isolated surrogate escapes throughout the object.
            json.dumps(row, ensure_ascii=False).encode("utf-8")
            columns, primary, keys = schemas[current]
            if row.keys() - columns.keys() or any(
                not valid(row.get(field), *rule) for field, rule in columns.items()
            ):
                reject("row violates SCHEMA")
            if primary is not None:
                key = row[primary]
                if key in keys:
                    reject("duplicate primary key")
                keys.add(key)
            tables[current].append(row)
    return tables


if __name__ == "__main__":
    try:
        print(json.dumps(read(sys.argv[1])[sys.argv[2]], ensure_ascii=False, allow_nan=False))
    except (ValueError, KeyError, OSError) as error:
        sys.exit(str(error))
