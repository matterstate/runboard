"""Local, synchronous reporting for Python training loops on Linux and macOS."""

import fcntl
import json
import math
import os
import time
import uuid
from collections.abc import Mapping
from contextlib import ExitStack
from pathlib import Path
from types import TracebackType
from typing import BinaryIO


class Run:
    """A locked training attempt belonging to a stable run. Use init()."""

    def __init__(
        self,
        directory: Path,
        name: str,
        config: Mapping[str, object] | None,
        resume_from: int | None = None,
    ) -> None:
        if resume_from is not None:
            _validate_step(resume_from)
        self._id = name
        self._attempt_id = uuid.uuid4().hex
        self._directory = directory.resolve()
        self._step = resume_from if resume_from is not None else -1
        metadata = _metadata(config or {})
        self.directory.mkdir(parents=True, exist_ok=True)
        with ExitStack() as resources:
            lock = resources.enter_context(
                (self.directory / f"{name}.lock").open("a+b")
            )
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise RuntimeError("run already has an active writer") from error
            if resume_from is not None:
                with (self.directory / f"{name}_meta.json").open("rb") as file:
                    body = file.read(8193)
                if len(body) > 8192:
                    raise ValueError("original config exceeds 8 KiB")
                previous = json.loads(body)
                if not isinstance(previous, dict) or not isinstance(
                    previous.get("parameters"), dict
                ):
                    raise ValueError("resume requires SDK-created run metadata")
                previous_metadata = _metadata(previous["parameters"])
                if config is not None:
                    if json.dumps(json.loads(metadata), sort_keys=True) != json.dumps(
                        json.loads(previous_metadata), sort_keys=True
                    ):
                        raise ValueError("resume config differs from the original run")
            else:
                for suffix in ("_meta.json", "_TRAINED"):
                    if (self.directory / f"{name}{suffix}").exists():
                        raise FileExistsError("run exists; use resume_from explicitly")
            self._log = resources.enter_context(
                (self.directory / f"{name}_train.log").open(
                    "r+b" if resume_from is not None else "x+b"
                )
            )
            self._elapsed_seconds = (
                _resume_seconds(self._log, resume_from)
                if resume_from is not None
                else 0.0
            )
            self._started = time.monotonic()
            event: dict[str, object] = {
                "runboard": {
                    "event": "resume" if resume_from is not None else "start",
                    "attempt": self.attempt_id,
                },
                "time": time.time(),
            }
            if resume_from is None:
                self._atomic_write("_meta.json", metadata)
            else:
                event.update(step=resume_from, seconds=self._elapsed_seconds)
                self._log.seek(0, os.SEEK_END)
                if self._log.tell():
                    self._log.seek(-1, os.SEEK_END)
                    if self._log.read(1) != b"\n":
                        self._log.write(b"\n")
                # Clear the old outcome before publishing the new attempt.
                (self.directory / f"{name}_TRAINED").unlink(missing_ok=True)
            self._write_record(event)
            self._resources = resources.pop_all()

    @property
    def id(self) -> str:
        return self._id

    @property
    def attempt_id(self) -> str:
        return self._attempt_id

    @property
    def directory(self) -> Path:
        return self._directory

    def __enter__(self) -> "Run":
        if self._log.closed:
            raise RuntimeError("run is closed")
        return self

    def __exit__(
        self,
        error_type: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        if self._log.closed:
            self._resources.close()
            return
        exit_code = 0 if error_type is None else 1
        if isinstance(error, KeyboardInterrupt):
            exit_code = 130
        try:
            try:
                self._write_record(
                    {
                        "runboard": {
                            "event": "finish",
                            "attempt": self.attempt_id,
                            "exit_code": exit_code,
                        },
                        "time": time.time(),
                    }
                )
                self._log.close()
                self._atomic_write("_TRAINED", f"train exit {exit_code}")
            finally:
                self._resources.close()
        except OSError as reporting_error:
            if error_type is None:
                raise
            if error is not None:
                error.add_note(f"Runboard finalization failed: {reporting_error}")

    def log(self, metrics: Mapping[str, float | int], *, step: int) -> None:
        """Write host scalars; repeated steps update keys, decreasing steps fail."""
        if self._log.closed:
            raise RuntimeError("run is closed")
        _validate_step(step)
        if step < self._step:
            raise ValueError("step decreased within the current attempt")
        if not 1 <= len(metrics) <= 32:
            raise ValueError("log requires 1 to 32 metrics")
        for key, value in metrics.items():
            if (
                not isinstance(key, str)
                or "\0" in key
                or not 1 <= len(key.encode()) <= 200
            ):
                raise ValueError(
                    "metric names must be NUL-free and contain 1 to 200 UTF-8 bytes"
                )
            if key in {"step", "time", "seconds", "monitor", "runboard"}:
                raise ValueError(f"reserved metric name: {key}")
            if key.startswith("runboard."):
                raise ValueError(f"reserved metric name: {key}")
            if type(value) not in (int, float):
                raise TypeError(f"{key}: expected a host int or float, not a tensor")
            if abs(value) > 1.7976931348623157e308 or not math.isfinite(value):
                raise ValueError(f"{key}: expected a finite number")
        self._write_record(
            {
                **metrics,
                "step": step,
                "time": time.time(),
                "seconds": self._elapsed_seconds + time.monotonic() - self._started,
            }
        )
        self._step = step

    def _write_record(self, record: Mapping[str, object]) -> None:
        body = (json.dumps(dict(record), allow_nan=False) + "\n").encode()
        if len(body) > 64 * 1024:
            raise ValueError("metric record exceeds 64 KiB")
        self._log.write(body)
        self._log.flush()

    def _atomic_write(self, suffix: str, body: str) -> None:
        target = self.directory / f"{self.id}{suffix}"
        temporary = target.with_name(target.name + ".tmp")
        try:
            temporary.write_text(body, encoding="utf-8")
            os.replace(temporary, target)
        finally:
            temporary.unlink(missing_ok=True)


def init(
    *,
    project: str,
    config: Mapping[str, object] | None = None,
    name: str | None = None,
    resume_from: int | None = None,
) -> Run:
    """Open a run, optionally restoring its recorded history through a checkpoint."""
    if resume_from is not None and name is None:
        raise ValueError("resume_from requires an explicit run name")
    name = name if name is not None else uuid.uuid4().hex
    for label in (project, name):
        if not label or len(label) > 200 or label.startswith("."):
            raise ValueError("invalid project or run name")
        if not all(
            character.isascii() and (character.isalnum() or character in "._-")
            for character in label
        ):
            raise ValueError("names accept ASCII letters, digits, '.', '_' and '-'")
    return Run(Path(".runboard") / project / "reports", name, config, resume_from)


def _validate_step(step: int) -> None:
    if type(step) is not int or not 0 <= step <= 2**32 - 1:
        raise ValueError("step must be an integer in [0, 2**32 - 1]")


def _resume_seconds(log: BinaryIO, step: int) -> float:
    """Use the latest recorded elapsed time on the retained checkpoint branch."""
    log.seek(0, os.SEEK_END)
    start = max(0, log.tell() - 16 * 1024 * 1024)
    boundary = True
    if start:
        log.seek(start - 1)
        boundary = log.read(1) == b"\n"
    log.seek(start)
    body = log.read(16 * 1024 * 1024)
    if not boundary:
        body = body.partition(b"\n")[2]
    for line in reversed(body.splitlines()):
        if len(line) > 64 * 1024:
            continue
        try:
            record = json.loads(line)
        except (ValueError, RecursionError):
            continue
        if not isinstance(record, dict):
            continue
        observed = record.get("step")
        if type(observed) is not int or not 0 <= observed <= 2**32 - 1:
            continue
        event = record.get("runboard")
        if isinstance(event, dict) and event.get("event") == "resume":
            step = min(step, observed)
        if observed <= step:
            seconds = record.get("seconds")
            if type(seconds) in (int, float):
                if 0 <= seconds <= 4_000_000_000 and math.isfinite(seconds):
                    return float(seconds)
    if start:
        raise ValueError("checkpoint predates the retained reporting history")
    return 0.0


def _metadata(config: Mapping[str, object]) -> str:
    metadata = json.dumps({"parameters": dict(config)}, allow_nan=False)
    if len(metadata.encode()) > 8192:
        raise ValueError("config exceeds 8 KiB")
    pending = [("parameters", json.loads(metadata)["parameters"])]
    fields = 0
    scalar_bytes = 0
    while pending:
        path, value = pending.pop()
        if "\0" in path or len(path.encode()) > 200 or path.count(".") >= 31:
            raise ValueError(
                "config paths require no NUL, at most 200 bytes and 30 dots"
            )
        if isinstance(value, dict):
            pending.extend((f"{path}.{key}", item) for key, item in value.items())
        elif isinstance(value, list):
            pending.extend(
                (f"{path}.{index}", item) for index, item in enumerate(value)
            )
        else:
            fields += 1
            if type(value) in (int, float):
                try:
                    number = float(value)
                except OverflowError as error:
                    raise ValueError("config numbers must fit finite floats") from error
                if not math.isfinite(number):
                    raise ValueError("config numbers must fit finite floats")
                rendered = (
                    format(number, ".3e")
                    if number and (abs(number) >= 10000 or abs(number) < 0.001)
                    else format(number, ".4f").rstrip("0").rstrip(".")
                )
            elif value is None:
                rendered = "null"
            elif isinstance(value, bool):
                rendered = str(value).lower()
            else:
                rendered = str(value)
            if "\0" in rendered:
                raise ValueError("config must not contain NUL")
            value_bytes = len(rendered.encode())
            scalar_bytes += len(path.encode()) + value_bytes
            if scalar_bytes > 16 * 1024:
                raise ValueError("config exceeds 16 KiB of scalar data")
            if fields > 128 or value_bytes > 1024:
                raise ValueError("config exceeds 128 fields or 1024 bytes per value")
        if len(pending) > 128:
            raise ValueError("config exceeds 128 pending fields")
    return metadata
