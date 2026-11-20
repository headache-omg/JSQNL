"""Internal reader for the JSQNL scan command."""
import datetime
import json
import os
import stat
import sys
from pathlib import Path

try:
    import pyarrow as pa
    import pyarrow.compute as pc
    import pyarrow.parquet as pq
except ImportError as error:
    sys.exit(f"Parquet: {error}. Set JSQNL_PYTHON to a Python with PyArrow installed.")


def json_default(value):
    if isinstance(value, (datetime.date, datetime.datetime)):
        return value.isoformat()
    raise TypeError(f"Unsupported Parquet value: {type(value).__name__}")


def contains_nanosecond_timestamp(kind):
    if pa.types.is_timestamp(kind):
        return kind.unit == "ns"
    if pa.types.is_dictionary(kind):
        return contains_nanosecond_timestamp(kind.value_type)
    return any(contains_nanosecond_timestamp(kind.field(i).type)
               for i in range(kind.num_fields))


def json_scalar(scalar):
    if not scalar.is_valid:
        return None
    kind = scalar.type
    if pa.types.is_timestamp(kind) and kind.unit == "ns":
        # Python datetime only preserves microseconds. Format in Arrow so all
        # nine fractional digits survive without requiring pandas.
        fmt = "%Y-%m-%dT%H:%M:%S"
        if kind.tz:
            text = pc.strftime(scalar, format=fmt + "%z", locale="C").as_py()
            return text[:-2] + ":" + text[-2:]
        return pc.strftime(scalar, format=fmt, locale="C").as_py()
    if pa.types.is_dictionary(kind):
        return json_scalar(scalar.value)
    if pa.types.is_struct(kind):
        names = [field.name for field in kind]
        if len(set(names)) != len(names):
            raise ValueError("Parquet struct field names must be unique")
        return {name: json_scalar(scalar[i]) for i, name in enumerate(names)}
    if pa.types.is_map(kind):
        return [(json_scalar(entry[0]), json_scalar(entry[1])) for entry in scalar.values]
    if (pa.types.is_list(kind) or pa.types.is_large_list(kind)
            or pa.types.is_fixed_size_list(kind)):
        return [json_scalar(item) for item in scalar.values]
    return scalar.as_py()


def json_rows(batch):
    columns = [
        [json_scalar(value) for value in array] if contains_nanosecond_timestamp(array.type)
        else array.to_pylist()
        for array in batch.columns
    ]
    for values in zip(*columns):
        yield dict(zip(batch.schema.names, values))


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
                    sys.stdout.buffer.write(b"{}\n")
                continue
            for batch in parquet.iter_batches(batch_size=256, columns=projection):
                if projection is not None:
                    # Arrow's Parquet projection treats dotted names as paths.
                    # Drop any incidental parent columns before converting values.
                    batch = batch.select(projection)
                for row in json_rows(batch):
                    line = json.dumps(row, default=json_default, ensure_ascii=False,
                                      allow_nan=False, separators=(",", ":"))
                    sys.stdout.buffer.write(line.encode("utf-8") + b"\n")


if __name__ == "__main__":
    try:
        scan(sys.argv[1], json.loads(sys.argv[2]), json.loads(sys.argv[3]))
    except (OSError, TypeError, ValueError) as error:
        sys.exit(f"Parquet: {error}")

