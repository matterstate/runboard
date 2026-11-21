"""Observable file protocol and failure contracts for training integrations."""

import contextlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import runboard


class ReportingTest(unittest.TestCase):
    def test_sparse_metrics_and_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory, contextlib.chdir(directory):
            with runboard.init(project="mnist", config={"steps": 2}) as run:
                run.log({"train/loss": 0.8}, step=1)
                run.log({"eval/accuracy_pct": 90.0}, step=1)
                run.log({"train/loss": 0.4}, step=2)
            reports = Path(".runboard/mnist/reports")
            records = [
                json.loads(line)
                for line in (reports / f"{run.id}_train.log").read_text().splitlines()
                if "runboard" not in json.loads(line)
            ]
            self.assertEqual([record["step"] for record in records], [1, 1, 2])
            self.assertNotIn("eval/accuracy_pct", records[-1])
            self.assertLessEqual(records[0]["seconds"], records[-1]["seconds"])
            self.assertGreater(records[0]["time"], 0)
            self.assertEqual(
                (reports / f"{run.id}_TRAINED").read_text(), "train exit 0"
            )
            self.assertEqual(
                json.loads((reports / f"{run.id}_meta.json").read_text()),
                {"parameters": {"steps": 2}},
            )
            with self.assertRaises(RuntimeError):
                run.log({"loss": 1}, step=3)

    def test_resume_preserves_config_prefix_time_and_partial_history(self) -> None:
        with tempfile.TemporaryDirectory() as directory, contextlib.chdir(directory):
            with patch("runboard.time.monotonic", return_value=10) as clock:
                with runboard.init(
                    project="resume", name="example", config={"lr": 1}
                ) as first:
                    clock.return_value = 20
                    first.log({"loss": 2, "eval": 0.8}, step=2)
                    clock.return_value = 30
                    first.log({"loss": 4, "abandoned": 1}, step=4)
            reports = Path(".runboard/resume/reports")
            log = reports / "example_train.log"
            with log.open("ab") as file:
                file.write(b'{"step":999')
            original = log.read_bytes()
            for options in ({"config": {"lr": 2}}, {"resume_from": -1}):
                arguments = {"project": "resume", "name": "example", "resume_from": 2}
                arguments.update(options)
                with self.assertRaises(ValueError):
                    runboard.init(**arguments)
                self.assertEqual(log.read_bytes(), original)
                self.assertTrue((reports / "example_TRAINED").exists())
            with patch("runboard.time.monotonic", return_value=100) as clock:
                with runboard.init(
                    project="resume", name="example", resume_from=2
                ) as second:
                    self.assertNotEqual(first.attempt_id, second.attempt_id)
                    self.assertEqual(first.id, second.id)
                    self.assertFalse((reports / "example_TRAINED").exists())
                    with self.assertRaises(ValueError):
                        second.log({"loss": 1}, step=1)
                    clock.return_value = 105
                    second.log({"loss": 0.5}, step=6)
            self.assertTrue(log.read_bytes().startswith(original + b"\n"))
            lines = log.read_bytes().splitlines()
            latest = json.loads(lines[-2])
            self.assertEqual(latest["seconds"], 15)
            self.assertEqual(
                json.loads(lines[-1])["runboard"],
                {"event": "finish", "attempt": second.attempt_id, "exit_code": 0},
            )
            metadata = json.loads((reports / "example_meta.json").read_text())
            self.assertEqual(metadata, {"parameters": {"lr": 1}})
            with self.assertRaises(ValueError):
                runboard.init(project="resume", resume_from=2)
            with log.open("r+b") as file:
                file.seek(17 * 1024 * 1024, os.SEEK_END)
                file.write(b"noise\n")
            size = log.stat().st_size
            with self.assertRaisesRegex(ValueError, "retained reporting history"):
                runboard.init(project="resume", name="example", resume_from=2)
            self.assertEqual(log.stat().st_size, size)
            self.assertEqual((reports / "example_TRAINED").read_text(), "train exit 0")

    def test_exclusive_writers_and_process_exit_release_the_lock(self) -> None:
        with tempfile.TemporaryDirectory() as directory, contextlib.chdir(directory):
            environment = dict(
                os.environ, PYTHONPATH=str(Path(runboard.__file__).parent.parent)
            )
            with runboard.init(project="locks", name="active"):
                subprocess.run(
                    [
                        sys.executable,
                        "-c",
                        """
import runboard
try:
    runboard.init(project="locks", name="active", resume_from=0)
except RuntimeError:
    pass
else:
    raise AssertionError("concurrent writer accepted")
""",
                    ],
                    env=environment,
                    check=True,
                    timeout=5,
                )
            subprocess.run(
                [
                    sys.executable,
                    "-c",
                    """
import os
import runboard
run = runboard.init(project="locks", name="crash")
run.log({"loss": 1}, step=1)
os._exit(0)
""",
                ],
                env=environment,
                check=True,
                timeout=5,
            )
            with runboard.init(project="locks", name="crash", resume_from=1) as resumed:
                resumed.log({"loss": 0.5}, step=2)
            original = RuntimeError("training failed")
            run = runboard.init(project="locks", name="io-error")
            with patch.object(run, "_atomic_write", side_effect=OSError("disk error")):
                with self.assertRaises(RuntimeError) as raised:
                    with run:
                        raise original
            self.assertIs(raised.exception, original)
            self.assertIn("Runboard finalization failed", original.__notes__[0])
            with runboard.init(project="locks", name="io-error", resume_from=0):
                pass

    def test_reader_limits_and_working_directory_changes(self) -> None:
        with tempfile.TemporaryDirectory() as directory, contextlib.chdir(directory):
            for config in (
                {"value": 10**400},
                {"x" * 180: {str(index): 1 for index in range(100)}},
                {"bad\0key": 1},
                {"value": "bad\0text"},
            ):
                with self.assertRaises(ValueError):
                    runboard.init(project="invalid", config=config)
            with runboard.init(project="cwd", name="example") as run:
                with self.assertRaises(ValueError):
                    run.log({"bad\0metric": 1}, step=0)
                elsewhere = Path("elsewhere").resolve()
                elsewhere.mkdir()
                # Keep the new cwd through finalization, not just through log().
                os.chdir(elsewhere)
                run.log({"loss": 1}, step=0)
            self.assertEqual(
                (Path(directory) / ".runboard/cwd/reports/example_TRAINED").read_text(),
                "train exit 0",
            )

    def test_failures_and_invalid_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory, contextlib.chdir(directory):
            original = RuntimeError("training failed")
            with self.assertRaises(RuntimeError) as raised:
                with runboard.init(project="test", name="failure") as run:
                    for metrics, step in [
                        ({"loss": float("nan")}, 0),
                        ({"step": 1}, 0),
                        ({"loss": True}, 0),
                        ({"loss": object()}, 0),
                        ({"loss": 1}, -1),
                    ]:
                        with self.assertRaises((TypeError, ValueError)):
                            run.log(metrics, step=step)
                    run.log({"loss": 1}, step=2)
                    with self.assertRaises(ValueError):
                        run.log({"loss": 1}, step=1)
                    raise original
            self.assertIs(raised.exception, original)
            final = json.loads(
                Path(".runboard/test/reports/failure_train.log")
                .read_bytes()
                .splitlines()[-1]
            )
            self.assertEqual(final["runboard"]["exit_code"], 1)
            self.assertEqual(
                Path(".runboard/test/reports/failure_TRAINED").read_text(),
                "train exit 1",
            )
            with self.assertRaises(FileExistsError):
                runboard.init(project="test", name="failure")
            for project in ("../escape", ".hidden", "", "a/b"):
                with self.assertRaises(ValueError):
                    runboard.init(project=project)


if __name__ == "__main__":
    unittest.main()
