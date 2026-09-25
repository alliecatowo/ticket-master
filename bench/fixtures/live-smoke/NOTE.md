# live-smoke bench fixture

This fixture backs `bench/tasks/live-smoke.toml`, the one task `tm bench run --live` uses to
prove its own plumbing (a real ticket created, activated, and run to completion; a cassette
recorded; real cost/tool-call numbers folded from the run) works end to end. `test_command` is
`true`, which always exits zero, so nothing here actually needs editing — this file exists only
so the fixture directory (and the scratch git history `--live` gives it) isn't empty.
