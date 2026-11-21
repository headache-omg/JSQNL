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


def needs_scalar_conversion(kind):
    if pa.types.is_time(kind):
        return True
    if pa.types.is_timestamp(kind):
        return kind.unit == "ns"
    if pa.types.is_dictionary(kind):
        return needs_scalar_conversion(kind.value_type)
    return any(needs_scalar_conversion(kind.field(i).type)
               for i in range(kind.num_fields))


TICKS = {"s": 1, "ms": 1000, "us": 10**6, "ns": 10**9}


def date_text(days):
    # Gregorian dates repeat every 400 years. Keep the conversion inside
    # datetime.date's supported range even for extended ISO years.
    cycles, remainder = divmod(days, 146097)
    date = datetime.date(1970, 1, 1) + datetime.timedelta(days=remainder)
    year = date.year + cycles * 400
    year_text = f"{year:04d}" if 0 <= year <= 9999 else f"{year:+07d}"
    return f"{year_text}-{date.month:02d}-{date.day:02d}"


def clock_text(seconds):
    hours, rest = divmod(seconds, 3600)
    minutes, seconds = divmod(rest, 60)
    return f"{hours:02d}:{minutes:02d}:{seconds:02d}"


def temporal_text(scalar):
    kind = scalar.type
    if pa.types.is_date32(kind):
        return date_text(scalar.cast(pa.int32()).as_py())
    if pa.types.is_date64(kind):
        return date_text(scalar.cast(pa.int64()).as_py() // 86400000)
    ticks = TICKS[kind.unit]
    seconds, fraction = divmod(scalar.value, ticks)
    suffix = ""
    if pa.types.is_timestamp(kind) and kind.tz:
        # Compute the offset at this exact UTC second. Historical offsets
        # can include seconds, which strftime("%z") truncates.
        utc = pa.scalar(seconds, type=pa.timestamp("s", kind.tz))
        local = pc.local_timestamp(utc).cast(pa.int64()).as_py()
        delta = local - seconds
        offset = clock_text(abs(delta))
        suffix = ("+" if delta >= 0 else "-") + (offset if delta % 60 else offset[:-3])
        seconds = local
    days, daytime = divmod(seconds, 86400)
    text = clock_text(daytime)
    if kind.unit == "ns":
        text += f".{fraction:09d}"
    elif fraction:
        text += f".{fraction * (10**6 // ticks):06d}"
    if pa.types.is_timestamp(kind):
        text = date_text(days) + "T" + text
    return text + suffix


def json_scalar(scalar):
    if not scalar.is_valid:
        return None
    kind = scalar.type
    if pa.types.is_timestamp(kind) or pa.types.is_date(kind) or pa.types.is_time(kind):
        return temporal_text(scalar)
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
    columns = []
    for array in batch.columns:
        if not needs_scalar_conversion(array.type):
            try:
                columns.append(array.to_pylist())
                continue
            except OverflowError:
                # Python datetime supports only years 1 through 9999.
                pass
        columns.append([json_scalar(value) for value in array])
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


def shard_identity(source):
    info = os.fstat(source.fileno())
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns)


def read_schema(path):
    with open_shard(path) as source:
        return pq.read_schema(source), shard_identity(source)


def scan(directory, required, projection):
    paths = sorted(Path(directory).glob("*.parquet"))
    if not paths:
        raise ValueError(f"No Parquet files in {directory}")

    identities = {}
    schema, identities[paths[0]] = read_schema(paths[0])
    if len(set(schema.names)) != len(schema.names) or "_line" in schema.names:
        raise ValueError("Parquet column names must be unique and cannot include _line")
    for column in required:
        if column not in schema.names:
            raise ValueError(f"unknown column {column}")

    # Validate all shard schemas before emitting query results.
    for path in paths[1:]:
        other_schema, identities[path] = read_schema(path)
        if not other_schema.equals(schema):
            raise ValueError(f"Parquet schema mismatch: {path}")

    for path in paths:
        with open_shard(path) as source:
            if shard_identity(source) != identities[path]:
                raise ValueError(f"Parquet shard changed during scan: {path}")
            with pq.ParquetFile(source) as parquet:
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
        required, projection = json.load(sys.stdin.buffer)
        scan(sys.argv[1], required, projection)
    except BrokenPipeError:
        os._exit(0)
    except (OSError, TypeError, ValueError, pa.ArrowException) as error:
        sys.exit(f"Parquet: {error}")

