# UI tests (Playwright + Python)

End-to-end tests that drive the SPA in a real browser against the
**host build**. Same HTML, same JSON API as the firmware, no hardware.

## One-time setup

```sh
python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
playwright install chromium
```

## Run

```sh
. .venv/bin/activate
pytest tests/playwright -v
```

The session fixture builds `cargo build --bin host`, picks a free port,
spawns the binary with `WC_HOST_BIND`, and tears it down on exit.

To watch the browser instead of running headless:

```sh
pytest tests/playwright --headed --slowmo 200
```

To run a single test file:

```sh
pytest tests/playwright/test_dashboard.py -v
```
