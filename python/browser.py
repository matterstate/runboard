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
        output.write(
            '{"step":5,"accuracy":0.5,"loss":0.5,"train/real_tokens":500,"train/flops_total":5000000000000}\n'
        )
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
    expect(page.locator("#live-apply")).to_be_disabled()
    page.wait_for_function("() => !updating && !dirty")

    figure = page.locator("figure").last
    chart = figure.locator("svg.chart")
    chart.focus()
    chart.press("End")
    expect(figure.locator("output")).to_contain_text(
        "demo/example (raw): 0.5 at tokens seen 1,500"
    )
    figure.get_by_role("button", name="Zoom in", exact=True).click()
    assert chart.get_attribute("data-zoomed") == "1"
    page.mouse.move(0, 0)
    page.evaluate(
        "document.activeElement.blur();"
        "window.original_chart = document.querySelectorAll('svg.chart')[1]"
    )
    update(page)
    expect(page.locator("#live-apply")).to_be_disabled()
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
    expect(page.locator("select[name=sort]")).to_have_value("recent")
    assert page.locator("#run-selection input[name=run]").evaluate_all(
        "nodes => nodes.slice(0, 3).map(node => node.value)"
    ) == ["demo/example", "demo/run-50", "demo/run-49"]
    task = page.get_by_role("link", name="Pueue #42", exact=True)
    task.click()
    expect(page.locator("#task-demo-42")).to_contain_text("Running")
    page.goto(address + "/?sort=name")
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
    comparison_url = page.url
    performance = page.locator("#training-performance")
    expect(performance.locator("th")).to_have_text(
        ["Run", "Mean ms/step", "Max ms/step", "Median tokens/sec"]
    )
    expect(performance.locator("tr").nth(1).locator("td")).to_have_text(
        ["demo/example", "25", "40", "2500"]
    )
    expect(performance.locator("tr").nth(2)).to_contain_text("Not reported")
    titles = [
        title.split(" ·", 1)[0]
        for title in page.locator("figure figcaption").all_text_contents()
    ]
    assert {
        "train/step_seconds_mean",
        "train/step_seconds_max",
        "train/tokens_per_second",
    } <= set(titles)
    for width in [390, 320]:
        page.set_viewport_size({"width": width, "height": 900})
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
        region = page.get_by_role("region", name="Training performance", exact=True)
        region.evaluate("node => node.scrollLeft = node.scrollWidth")
        assert region.evaluate("node => node.scrollLeft > 0")
    page.set_viewport_size({"width": 1280, "height": 900})
    page.goto(comparison_url + "&metric=missing")
    expect(page.locator("#training-performance tr").nth(1)).to_contain_text("2500")
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
        output.write(
            '{"step":6,"loss":0.25,"accuracy":0.6,"train/real_tokens":600,"train/flops_total":6000000000000}\n'
        )
    update(page)
    expect(cell).to_contain_text("0.25")
    after = [line.split(":")[0] for line in cell.inner_text().splitlines()]
    assert before == after

    for path in ["/?name=example", "/run?run=demo/example&metric=loss"]:
        page.goto(address + path)
        for width in [1600, 1280, 768, 390, 320]:
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
    page.goto(address + "/?name=mobile")
    name_cell = page.locator("#run-selection table tr").nth(1).locator("td").nth(1)
    assert 160 <= name_cell.bounding_box()["width"] <= 240
    expect(name_cell).to_contain_text("Pueue #43")
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
    page.screenshot(path=str(log.parent / "mobile-runs.png"), full_page=True)
    page.goto(address + "/run?run=demo/example&metric=missing")
    expect(page.locator(".empty")).to_contain_text("No matching metric")


def recovery(page: Page, address: str) -> None:
    page.goto(address + "/compare?run=demo/example&run=demo/run-50&metric=temperature")
    values = page.locator("#metric-values")
    expect(values.locator("tr")).to_have_count(33)
    page.get_by_role("link", name="Next metrics", exact=True).click()
    expect(values.locator("tr")).to_have_count(9)
    expect(values.locator("tr").nth(1)).to_contain_text("device/temperature_32")
    expect(values.locator("tr").nth(1).locator("td")).to_have_text(
        ["32", "Not reported"]
    )
    page.get_by_role("link", name="First metrics", exact=True).click()
    page.locator("#chart-filters input[name=metric]").fill("profile/block_39")
    page.locator("#chart-filters button").first.click()
    expect(values.locator("tr")).to_have_count(2)
    expect(values.locator("tr").nth(1).locator("td")).to_have_text(
        ["39", "Not reported"]
    )

    page.locator("#chart-filters input[name=metric]").fill("train/tokens_per_second")
    for aggregation, expected in [
        ("mean", "2500"),
        ("median", "2500"),
        ("min", "1000"),
        ("max", "4000"),
        ("latest", "4000"),
    ]:
        page.locator("select[name=aggregate]").select_option(aggregation)
        page.locator("#chart-filters button").first.click()
        expect(values.locator("tr").nth(1).locator("td")).to_have_text(
            [expected, "Not reported"]
        )
        expect(
            page.locator("#training-performance tr").nth(1).locator("td").last
        ).to_have_text("2500")

    budget_url = (
        address + "/compare?run=demo/example&run=demo/run-50&metric=loss&bandwidth=10"
    )
    page.goto(budget_url)
    titles = [
        text.split(" ·", 1)[0]
        for text in page.locator("figcaption").all_text_contents()
    ]
    loss = page.locator("figure").nth(titles.index("loss"))
    expect(loss.locator("svg.chart")).to_have_attribute("data-axis", "tokens seen")
    raw = loss.locator('polyline[data-label="demo/example (raw)"]')
    smoothed = loss.locator('polyline[data-label="demo/example (Gaussian)"]')
    raw_samples = json.loads(raw.get_attribute("data-samples"))
    smoothed_samples = json.loads(smoothed.get_attribute("data-samples"))
    assert raw_samples[0] == [100, 4]
    assert raw_samples[-1] == [2100, 0.25]
    assert smoothed_samples[-1][0] == 2100
    assert 0.25 < smoothed_samples[-1][1] < 4
    assert raw.get_attribute("stroke") == smoothed.get_attribute("stroke")
    assert float(raw.get_attribute("stroke-opacity")) < float(
        smoothed.get_attribute("stroke-opacity")
    )
    page.goto(budget_url + "&axis=flops")
    expect(loss.locator("svg.chart")).to_have_attribute("data-axis", "FLOPs")
    assert json.loads(raw.get_attribute("data-samples"))[-1] == [6e12, 0.25]
    expect(
        page.get_by_text("No loss observations paired with recorded FLOPs")
    ).to_be_visible()

    page.locator("#chart-filters input[name=metric]").fill("loss")
    page.locator("select[name=axis]").select_option("step")
    page.locator("#chart-options summary").click()
    page.locator("select[name=smoothing]").select_option("ema")
    page.locator("input[name=smooth]").fill("3")
    page.locator("#chart-filters button").first.click()
    expect(page.locator("select[name=smoothing]")).to_have_value("ema")
    titles = [
        text.split(" ·", 1)[0]
        for text in page.locator("figcaption").all_text_contents()
    ]
    samples = json.loads(
        page.locator("figure")
        .nth(titles.index("loss"))
        .locator('polyline[data-label="demo/example (EMA)"]')
        .get_attribute("data-samples")
    )
    assert samples[0] == [1, 4]
    assert samples[-1] == [6, 0.71875]
    row = page.locator("#metric-values tr").filter(
        has=page.get_by_role("rowheader", name="loss", exact=True)
    )
    expect(row.locator("td")).to_have_text(["0.25", "1"])
    loss.locator("[data-y-tick]").first.evaluate(
        "label => { label.textContent = '1.32984939427'; }"
    )
    page.set_viewport_size({"width": 390, "height": 900})
    page.wait_for_timeout(100)
    page.set_viewport_size({"width": 320, "height": 900})
    page.wait_for_timeout(100)
    assert loss.locator("svg.plot").evaluate(
        "plot => [...plot.querySelectorAll('[data-y-tick]')].every(label => "
        "label.getBoundingClientRect().left >= plot.getBoundingClientRect().left)"
    ), loss.locator("svg.plot").evaluate(
        "plot => ({left: plot.getBoundingClientRect().left, "
        "labels: [...plot.querySelectorAll('[data-y-tick]')].map(label => "
        "({text: label.textContent, left: label.getBoundingClientRect().left}))})"
    )

    page.clock.install(time=0)
    page.clock.pause_at(1000)
    page.set_viewport_size({"width": 320, "height": 900})
    page.goto(address + "/run?run=demo/example&metric=loss")
    page.mouse.move(0, 0)
    page.evaluate("Math.random = () => 0")
    assert page.evaluate("[retry_delay(0), retry_delay(100)]") == [500, 15000]
    page.evaluate("Math.random = () => 1")
    assert page.evaluate("[retry_delay(0), retry_delay(100)]") == [1000, 30000]
    page.evaluate("Math.random = () => 0")
    geometry = page.locator("nav").bounding_box()
    page.locator("#live-toggle").click()
    for delay in [500, 1000, 2000, 4000, 8000, 15000, 15000]:
        creations = page.evaluate("stream_creations")
        page.evaluate("test_events.onerror()")
        page.clock.run_for(delay - 1)
        assert page.evaluate("stream_creations") == creations
        page.clock.run_for(1)
        assert page.evaluate("stream_creations") == creations + 1
        expect(page.locator("#live-error")).to_be_hidden()
        assert page.locator("nav").bounding_box() == geometry
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")

    requests = []

    def temporary(route: Route) -> None:
        requests.append(route.request.url)
        if len(requests) <= 2:
            route.fulfill(status=503, body="temporarily unavailable")
        else:
            route.continue_()

    page.route("**/run?**", temporary)
    page.locator("main").evaluate('node => node.dataset.recoveryOld = "yes"')
    page.locator("#live-toggle").click()
    page.clock.run_for(250)
    expect(page.locator("#live-apply")).to_be_enabled()
    assert page.evaluate("snapshot_attempt") == 1
    page.clock.run_for(999)
    assert len(requests) == 1
    page.clock.run_for(1)
    expect(page.locator("#live-apply")).to_be_enabled()
    assert page.evaluate("snapshot_attempt") == 2
    page.clock.run_for(2000)
    expect(page.locator("main[data-recovery-old]")).to_have_count(0)
    assert len(requests) == 3
    expect(page.locator("#live-error")).to_be_hidden()
    assert page.locator("nav").bounding_box() == geometry
    page.unroute("**/run?**", temporary)

    def forbidden(route: Route) -> None:
        route.fulfill(status=403, body="access denied")

    page.route("**/run?**", forbidden)
    update(page)
    page.clock.run_for(250)
    expect(page.locator("#live-error")).to_contain_text("HTTP 403")
    creations = page.evaluate("stream_creations")
    page.clock.run_for(120000)
    assert page.evaluate("stream_creations") == creations
    assert page.locator("nav").bounding_box() == geometry
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
    page.unroute("**/run?**", forbidden)
    page.locator("main").evaluate('node => node.dataset.retryOld = "yes"')
    page.locator("#live-retry").click()
    page.clock.run_for(250)
    expect(page.locator("main[data-retry-old]")).to_have_count(0)
    expect(page.locator("#live-error")).to_be_hidden()
    page.clock.resume()


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
                        "train/step_seconds_mean": 0.025,
                        "train/step_seconds_max": 0.04,
                        "train/tokens_per_second": 1000 * step,
                        "train/real_tokens": 100 * step,
                        "train/flops_total": 1000000000000 * step,
                        **{f"profile/block_{index:02}": index for index in range(40)},
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
            path = reports / f"{name}_train.log"
            path.write_text('{"step":1,"loss":1,"train/tokens_seen":500}\n')
            modified = time.time() - 120 + index
            os.utime(path, (modified, modified))
            (reports / f"{name}_TRAINED").write_text("train exit 0")
        name = "mobile-" + "long-run-name-" * 12
        path = reports / f"{name}_train.log"
        path.write_text('{"step":1,"loss":1}\n')
        os.utime(path, (1, 1))
        (reports / f"{name}_meta.json").write_text('{"task_id":43}')
        (reports / "queue.json").write_text(
            json.dumps(
                {"tasks": {"42": {"label": "demo/example", "status": "Running"}}}
            )
        )
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
                            constructor() {
                                super(); window.test_events = this;
                                window.stream_creations =
                                    (window.stream_creations || 0) + 1;
                                queueMicrotask(() => this.onopen?.());
                            }
                            close() {}
                        };""")
                        interactions(page, address, log)
                        recovery(page, address)
                        assert not errors, errors
                        # Also exercise the real SSE connection.
                        live = browser.new_page()
                        live.goto(address + "/run?run=demo/example&metric=loss")
                        expect(live.locator("#live-status")).to_have_attribute(
                            "title", "Connected", timeout=10000
                        )
                        with log.open("a") as output:
                            output.write(
                                '{"step":7,"loss":0.125,"train/real_tokens":700}\n'
                            )
                        expect(live.locator("dl.metrics")).to_contain_text(
                            "0.125", timeout=10000
                        )
                        touch = browser.new_page(
                            viewport={"width": 390, "height": 900}, has_touch=True
                        )
                        touch.goto(address + "/run?run=demo/example&metric=loss")
                        touch.locator("svg.chart").last.tap()
                        expect(touch.locator("figure output").last).to_contain_text(
                            "demo/example (raw):"
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
