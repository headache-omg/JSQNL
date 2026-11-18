"""Internal reader for the JSQNL scan command."""
import datetime
import json
import os
import stat
import sys
from pathlib import Path

try:
    import pyarrow.parquet as pq
except ImportError as error:
    sys.exit(f"Parquet: {error}. Set JSQNL_PYTHON to a Python with PyArrow installed.")


def json_default(value):
    if isinstance(value, (datetime.date, datetime.datetime)):
        return value.isoformat()
    raise TypeError(f"Unsupported Parquet value: {type(value).__name__}")


def open_shard(path):
    def opener(name, flags):
        fd = os.open(name, flags | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
        try:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise ValueError(f"expected a regular Parquet file: {path}")
            return fd
        except BaseException:
            os.close(fd)
            raise

    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"expected a regular Parquet file: {path}")
    return open(path, "rb", opener=opener)


def read_schema(path):
    with open_shard(path) as source:
        return pq.read_schema(source)


def scan(directory, required, projection):
    paths = sorted(Path(directory).glob("*.parquet"))
    if not paths:
        raise ValueError(f"No Parquet files in {directory}")

    schema = read_schema(paths[0])
    if len(set(schema.names)) != len(schema.names) or "_line" in schema.names:
        raise ValueError("Parquet column names must be unique and cannot include _line")
    for column in required:
        if column not in schema.names:
            raise ValueError(f"unknown column {column}")

    # Validate all shard schemas before emitting query results.
    for path in paths[1:]:
        if not read_schema(path).equals(schema):
            raise ValueError(f"Parquet schema mismatch: {path}")

    for path in paths:
        with open_shard(path) as source, pq.ParquetFile(source) as parquet:
            if projection == []:
                for _ in range(parquet.metadata.num_rows):
                    sys.stdout.write("{}\n")
                continue
            for batch in parquet.iter_batches(batch_size=256, columns=projection):
                for row in batch.to_pylist():
                    sys.stdout.write(json.dumps(row, default=json_default, ensure_ascii=False,
                                                allow_nan=False, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    try:
        scan(sys.argv[1], json.loads(sys.argv[2]), json.loads(sys.argv[3]))
    except (OSError, TypeError, ValueError) as error:
        sys.exit(f"Parquet: {error}")
