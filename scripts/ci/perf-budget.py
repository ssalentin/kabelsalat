#!/usr/bin/env python3
"""Headless performance budget enforcement and release baseline recorder (#280).

Runs kabelsalat's headless benchmark subcommands across multiple runs, takes a
trimmed median to absorb shared CI runner variance, and checks every metric
against the per-architecture budgets and tolerances in docs/perf-budgets.json.

usage:
  scripts/ci/perf-budget.py --check --binary <path> [--budgets <path>] [--arch <arch>] [--json-out <path>]
  scripts/ci/perf-budget.py --record --binary <path> [--version <ver>] [--include-plugins] [--write] [--budgets <path>]
  scripts/ci/perf-budget.py --selftest [--budgets <path>]
"""
from __future__ import annotations

import contextlib
import copy
import datetime
import io
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
DEFAULT_BUDGETS = ROOT / "docs" / "perf-budgets.json"

SCENES = (
    "rack-front",
    "rack-back",
    "big-front",
    "big-back",
    "mixer",
    "big-mixer",
    "matrix",
)

REQUIRED_METRICS = (
    "engine.render_256.block_us",
    "engine.session.block_us",
    "engine.routing.block_us",
    *[f"ui.{scene}.frame_median_us" for scene in SCENES],
    *[f"ui.{scene}.vertices" for scene in SCENES],
    "session_load.total_median_ms",
    "session_load.apply_median_ms",
    "session_load.slowest_frame_median_ms",
    "startup.to_first_frame_ms",
    "memory.default_rack_rss_mib",
)


def die(msg: str) -> None:
    print(f"perf-budget.py: {msg}", file=sys.stderr)
    sys.exit(1)


def normalize_arch(machine: str | None = None) -> str:
    raw = (machine or platform.machine() or os.uname().machine).strip().lower()
    aliases = {
        "x86_64": "x86_64",
        "amd64": "x86_64",
        "aarch64": "aarch64",
        "arm64": "aarch64",
    }
    if raw not in aliases:
        die(f"unsupported architecture {raw!r} (expected x86_64 or aarch64)")
    return aliases[raw]


def trimmed_median(values: list[float]) -> float:
    if not values:
        raise ValueError("trimmed_median requires at least one value")
    ordered = sorted(float(v) for v in values)
    if len(ordered) >= 5:
        ordered = ordered[1:-1]
    return float(statistics.median(ordered))


def parse_json_stdout(stdout: str, label: str) -> dict:
    stripped = stdout.strip()
    if not stripped:
        raise ValueError(f"{label} produced empty stdout")
    try:
        data = json.loads(stripped)
        if isinstance(data, dict):
            return data
    except json.JSONDecodeError:
        pass
    # Plugins (e.g. Surge XT) can write banner lines to stdout before our JSON.
    decoder = json.JSONDecoder()
    for idx, ch in enumerate(stripped):
        if ch != "{":
            continue
        try:
            obj, end = decoder.raw_decode(stripped[idx:])
            if isinstance(obj, dict) and not stripped[idx + end :].strip():
                return obj
        except json.JSONDecodeError:
            continue
    raise ValueError(f"{label} did not emit a valid JSON object:\n{stripped[:500]}")


def isolated_env(home: pathlib.Path) -> dict[str, str]:
    env = os.environ.copy()
    home.mkdir(parents=True, exist_ok=True)
    for key, sub in (
        ("HOME", home),
        ("XDG_CONFIG_HOME", home / ".config"),
        ("XDG_CACHE_HOME", home / ".cache"),
        ("XDG_STATE_HOME", home / ".local" / "state"),
        ("XDG_DATA_HOME", home / ".local" / "share"),
    ):
        sub.mkdir(parents=True, exist_ok=True)
        env[key] = str(sub)
    env["KABELSALAT_WATCHDOG"] = "0"
    return env


def run_json_cmd(binary: pathlib.Path, args: list[str], env: dict[str, str], timeout_s: int = 180) -> dict:
    cmd = [str(binary), *args]
    done = subprocess.run(cmd, env=env, text=True, capture_output=True, timeout=timeout_s)
    if done.returncode != 0:
        raise RuntimeError(
            f"command {' '.join(cmd)} exited {done.returncode}\n"
            f"stdout:\n{done.stdout}\nstderr:\n{done.stderr}"
        )
    return parse_json_stdout(done.stdout, " ".join(args))


def run_headless_suite(
    binary: pathlib.Path,
    *,
    engine_runs: int = 3,
    engine_reps: int = 5,
    ui_runs: int = 3,
    ui_frames: int = 200,
    session_runs: int = 3,
    session_rounds: int = 5,
    startup_runs: int = 5,
) -> tuple[dict[str, float], dict]:
    samples: dict[str, list[float]] = {k: [] for k in REQUIRED_METRICS}
    hashes: dict[str, set[str]] = {"render_256": set(), "session": set(), "routing": set()}

    with tempfile.TemporaryDirectory(prefix="ksalat-perf-") as tmp:
        env = isolated_env(pathlib.Path(tmp) / "home")

        for _ in range(engine_runs):
            doc = run_json_cmd(
                binary,
                ["engine-bench", "render", "session", "routing", "--reps", str(engine_reps), "--json"],
                env,
            )
            r256 = next((r for r in doc.get("render", []) if r.get("buffer_frames") == 256), None)
            if not r256:
                raise RuntimeError("engine-bench JSON missing buffer_frames == 256 render row")
            samples["engine.render_256.block_us"].append(float(r256["block_us"]))
            hashes["render_256"].add(str(r256["hash"]))

            sess = doc.get("session")
            rout = doc.get("routing")
            if not sess or not rout:
                raise RuntimeError("engine-bench JSON missing session or routing section")
            samples["engine.session.block_us"].append(float(sess["block_us"]))
            samples["engine.routing.block_us"].append(float(rout["block_us"]))
            hashes["session"].add(str(sess["hash"]))
            hashes["routing"].add(str(rout["hash"]))

        for key, seen in hashes.items():
            if len(seen) != 1:
                raise RuntimeError(f"engine-bench {key} hash varied across runs: {sorted(seen)}")

        for _ in range(ui_runs):
            doc = run_json_cmd(binary, ["ui-bench", str(ui_frames), "--json"], env)
            by_name = {row["name"]: row for row in doc.get("scenes", [])}
            for scene in SCENES:
                if scene not in by_name:
                    raise RuntimeError(f"ui-bench JSON missing scene {scene!r}")
                row = by_name[scene]
                samples[f"ui.{scene}.frame_median_us"].append(float(row["frame_median_us"]))
                samples[f"ui.{scene}.vertices"].append(float(row["vertices"]))

        for _ in range(session_runs):
            doc = run_json_cmd(
                binary,
                ["session-load-bench", "--builtin", "--rounds", str(session_rounds), "--json"],
                env,
            )
            targets = doc.get("targets", [])
            if not targets:
                raise RuntimeError("session-load-bench JSON missing targets")
            row = targets[0]
            samples["session_load.total_median_ms"].append(float(row["total_median_ms"]))
            samples["session_load.apply_median_ms"].append(float(row["apply_median_ms"]))
            samples["session_load.slowest_frame_median_ms"].append(float(row["slowest_frame_median_ms"]))

        for _ in range(startup_runs):
            doc = run_json_cmd(binary, ["startup-bench", "--json"], env)
            samples["startup.to_first_frame_ms"].append(float(doc["to_first_frame_ms"]))
            samples["memory.default_rack_rss_mib"].append(float(doc["rss_mib"]))

    measured: dict[str, float] = {}
    for key, vals in samples.items():
        if key.endswith(".vertices"):
            measured[key] = float(max(vals))
        else:
            measured[key] = round(trimmed_median(vals), 3)

    meta = {
        "hashes": {k: next(iter(v)) for k, v in hashes.items()},
        "raw_samples": samples,
    }
    return measured, meta


def load_budgets(path: pathlib.Path) -> dict:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def validate_budgets_doc(doc: dict) -> list[str]:
    errors: list[str] = []
    tolerances = doc.get("tolerances")
    metric_areas = doc.get("metric_areas")
    budgets = doc.get("budgets")
    releases = doc.get("releases")

    if not isinstance(tolerances, dict):
        errors.append("missing tolerances object")
        return errors
    if not isinstance(metric_areas, dict):
        errors.append("missing metric_areas object")
        return errors
    if not isinstance(budgets, dict):
        errors.append("missing budgets object")
        return errors
    if not isinstance(releases, list) or not releases:
        errors.append("releases must be a non-empty list")

    req_set = set(REQUIRED_METRICS)
    if set(metric_areas.keys()) != req_set:
        missing = sorted(req_set - set(metric_areas.keys()))
        extra = sorted(set(metric_areas.keys()) - req_set)
        errors.append(f"metric_areas mismatch (missing={missing}, extra={extra})")

    for metric, info in metric_areas.items():
        area = info.get("area")
        if area not in tolerances:
            errors.append(f"metric {metric!r} references unknown tolerance area {area!r}")
        else:
            tol = tolerances[area]
            if not isinstance(tol, (int, float)) or not (0.0 < float(tol) < 1.0):
                errors.append(f"tolerance for area {area!r} must satisfy 0 < tol < 1, got {tol!r}")

    for arch in ("aarch64", "x86_64"):
        arch_budgets = budgets.get(arch)
        if not isinstance(arch_budgets, dict):
            errors.append(f"missing budgets.{arch}")
            continue
        if set(arch_budgets.keys()) != req_set:
            missing = sorted(req_set - set(arch_budgets.keys()))
            extra = sorted(set(arch_budgets.keys()) - req_set)
            errors.append(f"budgets.{arch} mismatch (missing={missing}, extra={extra})")
        for metric, val in arch_budgets.items():
            if not isinstance(val, (int, float)) or float(val) <= 0.0:
                errors.append(f"budgets.{arch}.{metric} must be > 0, got {val!r}")

    return errors


def evaluate(measured: dict[str, float], doc: dict, arch: str) -> tuple[int, list[dict], list[str]]:
    arch_budgets = doc["budgets"].get(arch)
    if not isinstance(arch_budgets, dict):
        raise ValueError(f"no budgets defined for architecture {arch!r}")
    metric_areas = doc["metric_areas"]
    tolerances = doc["tolerances"]

    rows: list[dict] = []
    failures: list[str] = []
    for metric, budget in arch_budgets.items():
        info = metric_areas[metric]
        area = info["area"]
        unit = info["unit"]
        tol = float(tolerances[area])
        budget_f = float(budget)
        ceiling = budget_f * (1.0 + tol)
        val = measured.get(metric)
        if val is None:
            status = "FAIL"
            failures.append(f"{metric}: missing from benchmark output (budget {budget_f:.2f} {unit})")
            rows.append({
                "metric": metric,
                "area": area,
                "unit": unit,
                "measured": None,
                "budget": budget_f,
                "tolerance": tol,
                "ceiling": ceiling,
                "ratio": None,
                "status": status,
            })
            continue
        val_f = float(val)
        ratio = val_f / budget_f
        if val_f > ceiling:
            status = "FAIL"
            failures.append(
                f"{metric}: {val_f:.2f} {unit} exceeds ceiling {ceiling:.2f} {unit} "
                f"(budget {budget_f:.2f} + {tol * 100:.0f}%, {ratio:.2f}x budget)"
            )
        elif val_f > budget_f:
            status = "WARN"
        else:
            status = "PASS"
        rows.append({
            "metric": metric,
            "area": area,
            "unit": unit,
            "measured": val_f,
            "budget": budget_f,
            "tolerance": tol,
            "ceiling": ceiling,
            "ratio": ratio,
            "status": status,
        })
    return (1 if failures else 0), rows, failures


def format_report(rows: list[dict], failures: list[str], arch: str) -> str:
    lines = [
        f"Performance budget check ({arch}):",
        f"  {'Metric':<38} {'Measured':>10} {'Budget':>10} {'Ceiling':>10} {'Unit':<8} {'Ratio':>6}  Status",
        f"  {'-' * 38} {'-' * 10} {'-' * 10} {'-' * 10} {'-' * 8} {'-' * 6}  ------",
    ]
    for r in rows:
        meas_s = "MISSING" if r["measured"] is None else (f"{r['measured']:.0f}" if r["unit"] == "vertices" else f"{r['measured']:.2f}")
        bud_s = f"{r['budget']:.0f}" if r["unit"] == "vertices" else f"{r['budget']:.2f}"
        ceil_s = f"{r['ceiling']:.0f}" if r["unit"] == "vertices" else f"{r['ceiling']:.2f}"
        ratio_s = "-" if r["ratio"] is None else f"{r['ratio']:.2f}x"
        lines.append(
            f"  {r['metric']:<38} {meas_s:>10} {bud_s:>10} {ceil_s:>10} {r['unit']:<8} {ratio_s:>6}  {r['status']}"
        )
    warns = sum(1 for r in rows if r["status"] == "WARN")
    passes = sum(1 for r in rows if r["status"] == "PASS")
    lines.append(
        f"Summary ({arch}): {passes} pass, {warns} within tolerance, {len(failures)} failed"
    )
    for msg in failures:
        lines.append(f"::error::{msg}")
    return "\n".join(lines)


def git_short_commit() -> str:
    try:
        done = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=True,
        )
        return done.stdout.strip()
    except Exception:
        return "unknown"


def crate_version() -> str:
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    for line in cargo.splitlines():
        if line.startswith("version = "):
            return line.split('"', 2)[1]
    return "0.0.0"


def record_release(
    binary: pathlib.Path,
    budgets_path: pathlib.Path,
    arch: str,
    version: str | None,
    include_plugins: bool,
    write: bool,
) -> int:
    measured, meta = run_headless_suite(binary)
    entry_measured: dict = {arch: measured}

    if include_plugins:
        env = os.environ.copy()
        env["KABELSALAT_WATCHDOG"] = "0"
        surge_block: dict = {}
        try:
            idle_doc = run_json_cmd(binary, ["engine-bench", "idle", "--json"], env, timeout_s=240)
            surge_block["engine_idle"] = idle_doc
        except Exception as exc:
            surge_block["engine_idle_error"] = str(exc)

        startup_samples = []
        for _ in range(5):
            try:
                startup_samples.append(run_json_cmd(binary, ["startup-bench", "--with-plugins", "--json"], env))
            except Exception as exc:
                surge_block["startup_with_plugins_error"] = str(exc)
                break
        if len(startup_samples) == 5:
            surge_block["startup_with_plugins"] = {
                "to_first_frame_ms": round(trimmed_median([s["to_first_frame_ms"] for s in startup_samples]), 3),
                "rss_mib": round(trimmed_median([s["rss_mib"] for s in startup_samples]), 3),
                "priv_dirty_mib": round(trimmed_median([s["priv_dirty_mib"] for s in startup_samples]), 3),
                "hwm_mib": round(trimmed_median([s["hwm_mib"] for s in startup_samples]), 3),
                "voices": startup_samples[0]["voices"],
            }
        entry_measured[f"{arch}_surge_xt"] = surge_block

    entry = {
        "version": version or f"{crate_version()}-dev",
        "date": datetime.date.today().isoformat(),
        "commit": git_short_commit(),
        "hashes": meta["hashes"],
        "measured": entry_measured,
    }
    print(json.dumps(entry, indent=2))
    if write:
        doc = load_budgets(budgets_path)
        releases = doc.setdefault("releases", [])
        existing = next((r for r in releases if r.get("version") == entry["version"]), None)
        if existing is not None:
            existing["date"] = entry["date"]
            existing["commit"] = entry["commit"]
            existing["hashes"] = entry["hashes"]
            existing.setdefault("measured", {}).update(entry_measured)
        else:
            releases.append(entry)
        budgets_path.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")
        print(f"Updated {budgets_path}", file=sys.stderr)
    return 0


def synthetic_budgets() -> dict:
    tolerances = {
        "render_time": 0.35,
        "ui_frame_time": 0.35,
        "ui_vertices": 0.10,
        "session_load_time": 0.40,
        "startup_time": 0.40,
        "memory_rss": 0.20,
    }
    metric_areas = {}
    arch_budgets = {}
    for m in REQUIRED_METRICS:
        if m.startswith("engine."):
            metric_areas[m] = {"area": "render_time", "unit": "us"}
            arch_budgets[m] = 100.0
        elif m.endswith(".vertices"):
            metric_areas[m] = {"area": "ui_vertices", "unit": "vertices"}
            arch_budgets[m] = 10000
        elif m.startswith("ui."):
            metric_areas[m] = {"area": "ui_frame_time", "unit": "us"}
            arch_budgets[m] = 2000.0
        elif m.startswith("session_load."):
            metric_areas[m] = {"area": "session_load_time", "unit": "ms"}
            arch_budgets[m] = 50.0
        elif m.startswith("startup."):
            metric_areas[m] = {"area": "startup_time", "unit": "ms"}
            arch_budgets[m] = 120.0
        else:
            metric_areas[m] = {"area": "memory_rss", "unit": "MiB"}
            arch_budgets[m] = 90.0
    return {
        "schema_version": 1,
        "tolerances": tolerances,
        "metric_areas": metric_areas,
        "budgets": {"aarch64": dict(arch_budgets), "x86_64": dict(arch_budgets)},
        "releases": [{"version": "0.0.0-test", "date": "2026-01-01", "commit": "0000000", "measured": {}}],
    }


def make_mock_binary(directory: pathlib.Path, name: str, *, render_mult: float = 1.0) -> pathlib.Path:
    script = directory / name
    script.write_text(
        f"""#!/usr/bin/env python3
import json, sys
mode = sys.argv[1]
if mode == "engine-bench":
    mult = {render_mult!r}
    print(json.dumps({{
        "bench": "engine-bench",
        "render": [{{"buffer_frames": 256, "block_us": 100.0 * mult, "hash": "1111111111111111"}}],
        "session": {{"block_us": 100.0 * mult, "hash": "2222222222222222"}},
        "routing": {{"block_us": 100.0 * mult, "hash": "3333333333333333"}},
    }}))
elif mode == "ui-bench":
    scenes = {list(SCENES)!r}
    print(json.dumps({{
        "bench": "ui-bench",
        "scenes": [
            {{"name": s, "frame_median_us": 1800.0, "vertices": 9500}}
            for s in scenes
        ],
    }}))
elif mode == "session-load-bench":
    print(json.dumps({{
        "bench": "session-load-bench",
        "targets": [{{"total_median_ms": 40.0, "apply_median_ms": 30.0, "slowest_frame_median_ms": 20.0}}],
    }}))
elif mode == "startup-bench":
    print(json.dumps({{
        "bench": "startup-bench",
        "to_first_frame_ms": 100.0,
        "rss_mib": 80.0,
    }}))
else:
    sys.exit(2)
""",
        encoding="utf-8",
    )
    script.chmod(0o755)
    return script


def selftest(budgets_path: pathlib.Path) -> int:
    doc = synthetic_budgets()
    baseline = {k: float(v) for k, v in doc["budgets"]["aarch64"].items()}

    # 1. At budget -> pass
    rc_pass, rows_pass, fail_pass = evaluate(baseline, doc, "aarch64")

    # 2. Within tolerance (+25% render vs 35% tolerance) -> pass with WARN
    warn_metrics = copy.deepcopy(baseline)
    warn_metrics["engine.session.block_us"] = baseline["engine.session.block_us"] * 1.25
    rc_warn, rows_warn, fail_warn = evaluate(warn_metrics, doc, "aarch64")
    warn_status = next(r["status"] for r in rows_warn if r["metric"] == "engine.session.block_us")

    # 3. Doubled reference rack render time (2x) -> FAIL
    doubled_render = copy.deepcopy(baseline)
    for k in ("engine.render_256.block_us", "engine.session.block_us", "engine.routing.block_us"):
        doubled_render[k] = baseline[k] * 2.0
    rc_double, _, fail_double = evaluate(doubled_render, doc, "aarch64")

    # 4. Tight vertex bound (+15% vs 10% tolerance) -> FAIL
    bad_verts = copy.deepcopy(baseline)
    bad_verts["ui.big-back.vertices"] = baseline["ui.big-back.vertices"] * 1.15
    rc_verts, _, _ = evaluate(bad_verts, doc, "aarch64")

    # 5. UI frame time, session load, startup, memory regressions -> FAIL
    bad_ui = copy.deepcopy(baseline)
    bad_ui["ui.big-front.frame_median_us"] *= 2.0
    rc_ui, _, _ = evaluate(bad_ui, doc, "aarch64")

    bad_sess = copy.deepcopy(baseline)
    bad_sess["session_load.total_median_ms"] *= 2.0
    rc_sess, _, _ = evaluate(bad_sess, doc, "aarch64")

    bad_start = copy.deepcopy(baseline)
    bad_start["startup.to_first_frame_ms"] *= 2.0
    rc_start, _, _ = evaluate(bad_start, doc, "aarch64")

    bad_mem = copy.deepcopy(baseline)
    bad_mem["memory.default_rack_rss_mib"] *= 1.30
    rc_mem, _, _ = evaluate(bad_mem, doc, "aarch64")

    # 6. Missing metric -> FAIL
    missing = copy.deepcopy(baseline)
    del missing["engine.session.block_us"]
    rc_missing, _, fail_missing = evaluate(missing, doc, "aarch64")

    # 7. Trimmed median drops single spike outlier
    med_val = trimmed_median([10.0, 10.2, 10.1, 10.3, 999.0])

    # 8. End-to-end CLI --check against a mock binary that doubles render time -> exit 1
    with tempfile.TemporaryDirectory(prefix="ksalat-selftest-") as tmp:
        tmp_dir = pathlib.Path(tmp)
        b_file = tmp_dir / "budgets.json"
        b_file.write_text(json.dumps(doc), encoding="utf-8")
        ok_bin = make_mock_binary(tmp_dir, "mock-kabelsalat-ok", render_mult=1.0)
        slow_bin = make_mock_binary(tmp_dir, "mock-kabelsalat-slow", render_mult=2.0)
        with contextlib.redirect_stdout(io.StringIO()):
            cli_ok = main(["--check", "--binary", str(ok_bin), "--budgets", str(b_file), "--arch", "aarch64"])
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            cli_fail = main(["--check", "--binary", str(slow_bin), "--budgets", str(b_file), "--arch", "aarch64"])
        cli_out = buf.getvalue()

    # 9. Validate real docs/perf-budgets.json if present on disk
    real_errors: list[str] = []
    if budgets_path.exists():
        real_errors = validate_budgets_doc(load_budgets(budgets_path))
    else:
        real_errors = [f"missing {budgets_path}"]

    # 10. Noise-tolerant JSON parser handles Surge-style stdout banner before JSON
    parsed_noisy = parse_json_stdout("Surge XT banner\n{\n  \"ok\": true\n}\n", "noisy")

    cases = [
        ("metrics at budget pass cleanly", rc_pass == 0 and not fail_pass and all(r["status"] == "PASS" for r in rows_pass)),
        ("metrics inside tolerance warn without failing", rc_warn == 0 and not fail_warn and warn_status == "WARN"),
        ("doubled render time (2x) fails check", rc_double == 1 and len(fail_double) == 3),
        ("UI vertex bloat (+15%) fails strict bound", rc_verts == 1),
        ("UI frame time regression (2x) fails", rc_ui == 1),
        ("session load regression (2x) fails", rc_sess == 1),
        ("startup time regression (2x) fails", rc_start == 1),
        ("default-rack RSS regression (+30%) fails", rc_mem == 1),
        ("missing metric fails check", rc_missing == 1 and any("missing" in f for f in fail_missing)),
        ("trimmed_median discards high/low spikes", abs(med_val - 10.2) < 1e-9),
        ("CLI --check passes nominal mock binary", cli_ok == 0),
        ("CLI --check exits non-zero on 2x render regression", cli_fail == 1 and "::error::engine.session.block_us" in cli_out),
        ("JSON parser skips plugin banner lines", parsed_noisy == {"ok": True}),
        (f"budgets file valid ({budgets_path.name}): {real_errors or 'ok'}", not real_errors),
    ]
    failed = [name for name, ok in cases if not ok]
    for name, ok in cases:
        print(f"  {'ok' if ok else 'FAIL'}: {name}")
    print(f"selftest: {len(cases) - len(failed)} ok, {len(failed)} failed")
    return 1 if failed else 0


def flag_value(argv: list[str], name: str, default: str | None = None) -> str | None:
    if name in argv:
        idx = argv.index(name)
        if idx + 1 >= len(argv):
            die(f"{name} requires a value")
        return argv[idx + 1]
    return default


def main(argv: list[str]) -> int:
    budgets_arg = flag_value(argv, "--budgets")
    budgets_path = pathlib.Path(budgets_arg) if budgets_arg else DEFAULT_BUDGETS

    if "--selftest" in argv:
        return selftest(budgets_path)

    if "--check" in argv:
        binary_arg = flag_value(argv, "--binary")
        if not binary_arg:
            die("--check requires --binary <path>")
        binary = pathlib.Path(binary_arg)
        if not binary.exists():
            die(f"binary not found: {binary}")
        arch = normalize_arch(flag_value(argv, "--arch"))
        doc = load_budgets(budgets_path)
        errs = validate_budgets_doc(doc)
        if errs:
            die(f"invalid budgets file {budgets_path}: {'; '.join(errs)}")
        measured, meta = run_headless_suite(binary)
        rc, rows, failures = evaluate(measured, doc, arch)
        print(format_report(rows, failures, arch))
        json_out = flag_value(argv, "--json-out")
        if json_out:
            pathlib.Path(json_out).write_text(
                json.dumps({"arch": arch, "measured": measured, "hashes": meta["hashes"], "rows": rows}, indent=2) + "\n",
                encoding="utf-8",
            )
        return rc

    if "--record" in argv:
        binary_arg = flag_value(argv, "--binary")
        if not binary_arg:
            die("--record requires --binary <path>")
        binary = pathlib.Path(binary_arg)
        if not binary.exists():
            die(f"binary not found: {binary}")
        arch = normalize_arch(flag_value(argv, "--arch"))
        return record_release(
            binary,
            budgets_path,
            arch,
            flag_value(argv, "--version"),
            "--include-plugins" in argv,
            "--write" in argv,
        )

    die(__doc__[__doc__.index("usage:") :].strip())
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
