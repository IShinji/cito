# Security

Plain `cito collect` is static: it parses source files and does not import
or execute your code. Two things do run code, so treat the project (and its
conftest.py, plugins, and dependencies) as trusted before using them:

- `cito run` (and `--warm`, `--daemon`, watch mode) executes your tests by
  invoking pytest in your project's Python environment.
- `--python <interpreter>` makes collection probe that interpreter, which
  really imports the modules named in `pytest.importorskip(...)` calls, so
  their import-time side effects run.

To report a vulnerability, use GitHub's private vulnerability reporting on
this repository (Security → Report a vulnerability). Reports are read
promptly; please do not open public issues for suspected vulnerabilities.
