"""Exercise live dashboard interactions against synthetic, local reports."""

import json
import os
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import Page, Route, expect, sync_playwright


def available_port() -> int:
    with socket.socket() as connection:
        connection.bind(("127.0.0.1", 0))
        return connection.getsockname()[1]


def update(page: Page) -> None:
    page.evaluate("""() => {
        window.test_sequence = (window.test_sequence || 0) + 1;
        test_events.dispatchEvent(new MessageEvent('update', {
            lastEventId: String(test_sequence)
        }));
    }""")


def interactions(page: Page, address: str, log: Path) -> None:
    page.goto(address + "/run?run=demo/example")
    assert "loss" in page.locator("dl.metrics dt").all_text_contents()
    assert "loss" in [
        title.split(" ·", 1)[0]
        for title in page.locator("figure figcaption").all_text_contents()
    ]
    page.evaluate("""() => {
        window.original_main = document.querySelector('main');
        const range = document.createRange();
        range.selectNodeContents(document.querySelector('h1'));
        getSelection().removeAllRanges();
        getSelection().addRange(range);
    }""")
    selected = page.evaluate("getSelection().toString()")
    assert selected == "demo/example", (selected, page.locator("h1").inner_text())
    with log.open("a") as output:
        output.write('{"step":5,"accuracy":0.5,"loss":0.5}\n')
    update(page)
    expect(page.locator("#live-apply")).to_be_visible()
    page.wait_for_timeout(600)
    selected = page.evaluate("getSelection().toString()")
    assert selected == "demo/example", repr(selected)
    assert page.evaluate("original_main === document.querySelector('main')")
    page.evaluate("getSelection().removeAllRanges()")
    page.wait_for_function("() => original_main !== document.querySelector('main')")

    metric = page.locator("#chart-filters input[name=metric]")
    metric.fill("lo")
    metric.evaluate("element => element.setSelectionRange(1, 2)")
    page.evaluate("window.original_input = document.activeElement")
    update(page)
    page.wait_for_timeout(600)
    assert page.evaluate("original_input === document.activeElement")
    assert metric.input_value() == "lo"
    assert metric.evaluate(
        "element => [element.selectionStart, element.selectionEnd]"
    ) == [
        1,
        2,
    ]
    metric.fill("LOSS")
    page.locator("#chart-filters button").click()
    expect(page.locator("figure")).to_have_count(2)
    assert (
        page.locator("figure figcaption").all_text_contents()[0].startswith("eval/loss")
    )

    # An interaction beginning during a fetch must also prevent replacement.
    page.mouse.move(0, 0)
    page.evaluate(
        "document.activeElement.blur();"
        "window.original_main = document.querySelector('main')"
    )

    def edit_during_fetch(route: Route) -> None:
        metric.fill("draft")
        route.continue_()

    page.route("**/run?*", edit_during_fetch)
    update(page)
    expect(metric).to_have_value("draft")
    page.wait_for_timeout(600)
    assert page.evaluate("original_main === document.querySelector('main')")
    page.unroute("**/run?*", edit_during_fetch)
    page.locator("#live-apply").click()
    expect(page.locator("#live-apply")).to_be_hidden()
    page.wait_for_function("() => !updating && !dirty")

    figure = page.locator("figure").last
    chart = figure.locator("svg.chart")
    chart.focus()
    chart.press("End")
    expect(figure.locator("output")).to_contain_text("demo/example: 0.5 at step 5")
    figure.get_by_role("button", name="Zoom in", exact=True).click()
    assert chart.get_attribute("data-zoomed") == "1"
    page.mouse.move(0, 0)
    page.evaluate(
        "document.activeElement.blur();"
        "window.original_chart = document.querySelectorAll('svg.chart')[1]"
    )
    update(page)
    expect(page.locator("#live-apply")).to_be_hidden()
    page.wait_for_function("() => !updating && !dirty")
    assert page.evaluate("original_chart === document.querySelectorAll('svg.chart')[1]")
    figure.get_by_role("button", name="Reset zoom").click()
    page.mouse.move(0, 0)
    page.evaluate("document.activeElement.blur()")
    page.wait_for_function("() => !updating && !dirty")
    assert chart.get_attribute("data-zoomed") is None

    page.locator("#live-toggle").click()
    page.evaluate("window.original_main = document.querySelector('main')")
    update(page)
    page.wait_for_timeout(600)
    assert page.evaluate("original_main === document.querySelector('main')")
    expect(page.locator("#live-status")).to_contain_text("paused")
    page.locator("#live-toggle").click()
    page.wait_for_function("() => original_main !== document.querySelector('main')")

    page.goto(address)
    expect(page.locator("input[name=name]")).to_be_visible()
    preview = (
        page.locator("table tr")
        .filter(has=page.locator('input[value="demo/example"]'))
        .locator("td")
        .nth(4)
    )
    expect(preview).to_contain_text("loss:")
    page.locator('input[name=run][value="demo/example"]').check()
    page.get_by_role("link", name="Next", exact=True).click()
    page.locator('input[name=run][value="demo/run-50"]').check()
    expect(page.locator("#selection-count")).to_have_text("2 selected (up to eight)")
    page.get_by_role("button", name="Compare selected", exact=True).click()
    assert page.url.count("run=") == 2
    assert "/compare?" in page.url
    page.goto(page.url + "&metric=missing")
    expect(page.locator(".empty")).to_contain_text("No matching metric")
    page.goto(address + "/?name=example")
    expect(page.locator('input[name=run][value="demo/example"]')).to_be_checked()
    page.locator("#selection-clear").click()
    expect(
        page.get_by_role("button", name="Compare selected", exact=True)
    ).to_be_disabled()

    cell = page.locator("table tr").nth(1).locator("td").nth(4)
    before = [line.split(":")[0] for line in cell.inner_text().splitlines()]
    page.mouse.move(0, 0)
    page.evaluate("document.activeElement.blur()")
    with log.open("a") as output:
        output.write('{"step":6,"loss":0.25,"accuracy":0.6}\n')
    update(page)
    expect(cell).to_contain_text("0.25")
    after = [line.split(":")[0] for line in cell.inner_text().splitlines()]
    assert before == after

    for path in ["/?name=example", "/run?run=demo/example&metric=loss"]:
        page.goto(address + path)
        for width in [1600, 1280, 390]:
            page.set_viewport_size({"width": width, "height": 900})
            page.wait_for_function("""() => {
                const plot = document.querySelector('svg.plot');
                return !plot || Math.abs(plot.viewBox.baseVal.width -
                    plot.getBoundingClientRect().width) < 1;
            }""")
            assert page.evaluate(
                "document.documentElement.scrollWidth <= innerWidth"
            ), f"document overflow on {path} at {width}px"
            if path.startswith("/?"):
                assert page.evaluate("""() => {
                    const form = document.querySelector('#run-selection');
                    const row = form.querySelector('tr').getBoundingClientRect();
                    return form.clientWidth >= innerWidth - 48 &&
                        (innerWidth < 600 ||
                            Math.abs(row.width - form.clientWidth) < 2);
                }"""), f"runs table does not fill available width at {width}px"
    page.goto(address + "/run?run=demo/example&metric=missing")
    expect(page.locator(".empty")).to_contain_text("No matching metric")


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="runboard-browser-") as directory:
        root = Path(directory)
        reports = root / "reports"
        reports.mkdir()
        log = reports / "example_train.log"
        log.write_text(
            "".join(
                json.dumps(
                    {
                        "step": step,
                        "loss": 5 - step,
                        "accuracy": step / 10,
                        "eval/loss": 5.5 - step,
                        **{
                            f"device/temperature_{index:02}": index
                            for index in range(40)
                        },
                    }
                )
                + "\n"
                for step in range(1, 5)
            )
        )
        for index in range(51):
            name = f"run-{index:02}"
            (reports / f"{name}_train.log").write_text('{"step":1,"loss":1}\n')
            (reports / f"{name}_TRAINED").write_text("train exit 0")
        address = f"http://127.0.0.1:{available_port()}"
        with (root / "server.log").open("w") as output:
            process = subprocess.Popen(
                [
                    os.environ["RUNBOARD_BIN"],
                    "--root",
                    f"demo={root}",
                    "--listen",
                    address.removeprefix("http://"),
                    "--state-dir",
                    str(root / "state"),
                    "--nats-port",
                    str(available_port()),
                ],
                stdout=output,
                stderr=output,
                start_new_session=True,
            )
            try:
                for _ in range(100):
                    try:
                        with urllib.request.urlopen(address + "/healthz", timeout=1):
                            break
                    except OSError:
                        if process.poll() is not None:
                            raise RuntimeError((root / "server.log").read_text())
                        time.sleep(0.1)
                else:
                    raise TimeoutError("Runboard did not become healthy")
                with sync_playwright() as playwright:
                    browser = playwright.chromium.launch(
                        executable_path=os.environ["CHROMIUM_BIN"],
                        headless=True,
                        chromium_sandbox=False,
                    )
                    try:
                        page = browser.new_page()
                        errors: list[str] = []
                        page.on("pageerror", lambda error: errors.append(str(error)))
                        page.add_init_script("""
                        window.EventSource = class extends EventTarget {
                            constructor() { super(); window.test_events = this; }
                        };""")
                        interactions(page, address, log)
                        assert not errors, errors
                        # Also exercise the real SSE connection.
                        live = browser.new_page()
                        live.goto(address + "/run?run=demo/example&metric=loss")
                        expect(live.locator("#live-status")).to_have_text(
                            "Connected", timeout=10000
                        )
                        with log.open("a") as output:
                            output.write('{"step":7,"loss":0.125}\n')
                        expect(live.locator("dl.metrics")).to_contain_text(
                            "0.125", timeout=10000
                        )
                        touch = browser.new_page(
                            viewport={"width": 390, "height": 900}, has_touch=True
                        )
                        touch.goto(address + "/run?run=demo/example&metric=loss")
                        touch.locator("svg.chart").last.tap()
                        expect(touch.locator("figure output").last).to_contain_text(
                            "demo/example:"
                        )
                    finally:
                        browser.close()
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
    print("Browser UX checks passed")


if __name__ == "__main__":
    main()
