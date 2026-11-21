"""Local, synchronous reporting for Python training loops."""

import json
import math
import os
import time
import uuid
from collections.abc import Mapping
from pathlib import Path
from types import TracebackType


class Run:
    """A single training attempt. Use init() and a with statement."""

    def __init__(
        self, directory: Path, name: str, config: Mapping[str, object]
    ) -> None:
        self.id = name
        self.directory = directory
        self._step = -1
        self._closed = False
        self._started = time.monotonic()
        metadata = json.dumps({"parameters": dict(config)}, allow_nan=False)
        if len(metadata.encode()) > 8192:
            raise ValueError("config exceeds 8 KiB")
        pending = [("parameters", json.loads(metadata)["parameters"])]
        fields = 0
        while pending:
            path, value = pending.pop()
            if len(path.encode()) > 200 or path.count(".") >= 31:
                raise ValueError("config field path exceeds 200 bytes or 31 levels")
            if isinstance(value, dict):
                pending.extend((f"{path}.{key}", item) for key, item in value.items())
            elif isinstance(value, list):
                pending.extend(
                    (f"{path}.{index}", item) for index, item in enumerate(value)
                )
            else:
                fields += 1
                if fields > 128 or len(str(value).encode()) > 1024:
                    raise ValueError(
                        "config exceeds 128 fields or 1024 bytes per value"
                    )
            if len(pending) > 128:
                raise ValueError("config exceeds 128 pending fields")
        directory.mkdir(parents=True, exist_ok=True)
        # Exclusive creation prevents accidentally appending to another attempt.
        self._log = (directory / f"{name}_train.log").open("x", encoding="utf-8")
        try:
            self._atomic_write("_meta.json", metadata)
        except BaseException:
            self._log.close()
            raise

    def __enter__(self) -> "Run":
        return self

    def __exit__(
        self,
        error_type: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        try:
            self._log.close()
            outcome = "train exit 0" if error_type is None else "train exit 1"
            if isinstance(error, KeyboardInterrupt):
                outcome = "train exit 130"
            self._atomic_write("_TRAINED", outcome)
        except OSError as reporting_error:
            if error_type is None:
                raise
            if error is not None:
                error.add_note(f"Runboard finalization failed: {reporting_error}")
        finally:
            self._closed = True

    def log(self, metrics: Mapping[str, float | int], *, step: int) -> None:
        """Write host scalars; repeated steps update keys, decreasing steps fail."""
        if self._closed:
            raise RuntimeError("run is closed")
        if type(step) is not int or not 0 <= step <= 2**32 - 1:
            raise ValueError("step must be an integer in [0, 2**32 - 1]")
        if step < self._step:
            raise ValueError("step decreased; checkpoint resume is not supported yet")
        if not 1 <= len(metrics) <= 32:
            raise ValueError("log requires 1 to 32 metrics")
        for key, value in metrics.items():
            if not isinstance(key, str) or not 1 <= len(key.encode()) <= 200:
                raise ValueError("metric names must contain 1 to 200 UTF-8 bytes")
            if key in {"step", "time", "seconds", "monitor"}:
                raise ValueError(f"reserved metric name: {key}")
            if type(value) not in (int, float):
                raise TypeError(f"{key}: expected a host int or float, not a tensor")
            if abs(value) > 1.7976931348623157e308 or not math.isfinite(value):
                raise ValueError(f"{key}: expected a finite number")
        record = {
            **metrics,
            "step": step,
            "time": time.time(),
            "seconds": time.monotonic() - self._started,
        }
        body = json.dumps(record, allow_nan=False) + "\n"
        if len(body.encode()) > 64 * 1024:
            raise ValueError("metric record exceeds 64 KiB")
        self._log.write(body)
        self._log.flush()
        self._step = step

    def _atomic_write(self, suffix: str, body: str) -> None:
        target = self.directory / f"{self.id}{suffix}"
        temporary = target.with_name(target.name + ".tmp")
        try:
            temporary.write_text(body, encoding="utf-8")
            os.replace(temporary, target)
        finally:
            temporary.unlink(missing_ok=True)


def init(
    *, project: str, config: Mapping[str, object] | None = None, name: str | None = None
) -> Run:
    """Create a fresh run under .runboard/<project>/reports in the working directory."""
    name = name if name is not None else uuid.uuid4().hex
    for label in (project, name):
        if not label or len(label) > 200 or label.startswith("."):
            raise ValueError("invalid project or run name")
        if not all(
            character.isascii() and (character.isalnum() or character in "._-")
            for character in label
        ):
            raise ValueError("names accept ASCII letters, digits, '.', '_' and '-'")
    return Run(Path(".runboard") / project / "reports", name, config or {})
