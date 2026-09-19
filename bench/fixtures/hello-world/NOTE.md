# hello-world bench fixture

This file exists so `bench/tasks/hello-world.toml`'s `FileExists` predicate has something real
to check. It is attested by `script.txt` in this same directory, which `tm bench run` replays
verbatim as the task's single scripted step.
