"""CPU-only reporting sketch, not an MLPerf benchmark or compliance logger.

Uses the separate training/evaluation reporting sites from tinygrad's MLPerf
examples. Real training supplies already-materialized values at these sites.
"""

import runboard


def main() -> None:
    config = {"epochs": 2, "steps_per_epoch": 3, "batch_size": 512}
    with runboard.init(project="mlperf-example", config=config) as run:
        for epoch in range(config["epochs"]):
            for index in range(config["steps_per_epoch"]):
                step = epoch * config["steps_per_epoch"] + index + 1
                # In a real loop, log after the measured step interval ends.
                run.log(
                    {
                        "train/loss": 1.0 / step,
                        "train/top_1_acc": step / 10,
                        "epoch": epoch + (index + 1) / config["steps_per_epoch"],
                    },
                    step=step,
                )
            run.log({"eval/loss": 0.5 / step, "eval/top_1_acc": step / 10}, step=step)
        print(f"Run: {run.id}; view with runboard --root .runboard/mlperf-example")


if __name__ == "__main__":
    main()
