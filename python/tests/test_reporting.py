"""Observable file protocol and failure contracts for training integrations."""

import contextlib
import json
import tempfile
import unittest
from pathlib import Path

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
