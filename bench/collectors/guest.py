from __future__ import annotations

import json
import shlex
from pathlib import Path
from typing import Any


GUEST_OUTPUT_DIR = "/scx_bench_out"


def build_guest_script(
    bench_command: list[str],
    env: dict[str, str] | None = None,
    scheduler: dict[str, Any] | None = None,
    output_dir: str = GUEST_OUTPUT_DIR,
) -> str:
    scheduler = scheduler or {"kind": "builtin"}
    command = _shell_command(bench_command, env or {})
    scheduler_command = _scheduler_command(scheduler)

    return f"""#!/bin/sh
set +e

OUT={shlex.quote(output_dir)}
BENCH_CMD={shlex.quote(command)}
SCHEDULER_KIND={shlex.quote(str(scheduler.get("kind", "builtin")))}
SCHEDULER_CMD={shlex.quote(scheduler_command)}
scheduler_pid=""
scheduler_start_rc=0

mkdir -p "$OUT/snapshots/before" "$OUT/snapshots/after" "$OUT/snapshots/delta"

copy_file() {{
    src="$1"
    dst="$2"
    if [ -e "$src" ]; then
        cat "$src" > "$dst" 2>"$dst.err" || true
    fi
}}

copy_sched_ext() {{
    phase="$1"
    src="/sys/kernel/debug/sched_ext"
    dst="$OUT/snapshots/$phase/sched_ext"
    mkdir -p "$dst"

    mount -t debugfs none /sys/kernel/debug 2>/dev/null || true
    if [ ! -d "$src" ]; then
        return 0
    fi

    find "$src" -maxdepth 3 -type f 2>/dev/null | while read file; do
        rel="${{file#$src/}}"
        rel_dir="$(dirname "$rel")"
        mkdir -p "$dst/$rel_dir"
        cat "$file" > "$dst/$rel" 2>"$dst/$rel.err" || true
    done
}}

snapshot() {{
    phase="$1"
    dir="$OUT/snapshots/$phase"
    mkdir -p "$dir"

    copy_file /proc/stat "$dir/proc_stat.txt"
    copy_file /proc/schedstat "$dir/proc_schedstat.txt"
    copy_file /proc/interrupts "$dir/proc_interrupts.txt"
    copy_file /proc/pressure/cpu "$dir/psi_cpu.txt"
    copy_file /proc/pressure/io "$dir/psi_io.txt"
    copy_file /proc/pressure/memory "$dir/psi_memory.txt"
    dmesg > "$dir/dmesg.txt" 2>"$dir/dmesg.err" || true
    copy_sched_ext "$phase"
}}

start_scheduler() {{
    if [ "$SCHEDULER_KIND" = "builtin" ]; then
        return 0
    fi

    if [ "$SCHEDULER_KIND" != "scx" ]; then
        echo "unsupported scheduler kind: $SCHEDULER_KIND" > "$OUT/scheduler_stderr.log"
        return 125
    fi

    (
        eval "$SCHEDULER_CMD"
    ) > "$OUT/scheduler_stdout.log" 2> "$OUT/scheduler_stderr.log" &
    scheduler_pid="$!"
    sleep 1
    if ! kill -0 "$scheduler_pid" 2>/dev/null; then
        wait "$scheduler_pid"
        return "$?"
    fi
    return 0
}}

stop_scheduler() {{
    if [ -n "$scheduler_pid" ] && kill -0 "$scheduler_pid" 2>/dev/null; then
        kill "$scheduler_pid" 2>/dev/null || true
        wait "$scheduler_pid" 2>/dev/null || true
    fi
}}

snapshot before
start_epoch="$(date +%s)"
export SCX_BENCH_OUT="$OUT"

start_scheduler
scheduler_start_rc="$?"

if [ "$scheduler_start_rc" -eq 0 ]; then
    (
        eval "$BENCH_CMD"
    ) > "$OUT/stdout.log" 2> "$OUT/stderr.log"
    bench_rc="$?"
else
    bench_rc=125
    : > "$OUT/stdout.log"
    echo "scheduler failed to start" > "$OUT/stderr.log"
fi

stop_scheduler

end_epoch="$(date +%s)"
snapshot after

if command -v diff >/dev/null 2>&1; then
    diff -u "$OUT/snapshots/before/dmesg.txt" "$OUT/snapshots/after/dmesg.txt" \\
        > "$OUT/snapshots/delta/dmesg.diff" 2>/dev/null || true
fi

cat > "$OUT/guest_result.json" <<EOF
{{
  "scheduler_start_returncode": $scheduler_start_rc,
  "bench_returncode": $bench_rc,
  "started_at_epoch": $start_epoch,
  "finished_at_epoch": $end_epoch
}}
EOF

exit "$bench_rc"
"""


def write_guest_script(
    path: Path,
    bench_command: list[str],
    env: dict[str, str] | None = None,
    scheduler: dict[str, Any] | None = None,
    output_dir: str = GUEST_OUTPUT_DIR,
) -> None:
    path.write_text(
        build_guest_script(bench_command, env, scheduler, output_dir),
        encoding="utf-8",
    )
    path.chmod(0o755)


def parse_bench_metrics(stdout_path: Path) -> dict[str, Any]:
    if not stdout_path.exists():
        return {"metrics": {}, "metadata": {}, "parse_status": "missing_stdout"}

    text = stdout_path.read_text(encoding="utf-8", errors="replace").strip()
    if not text:
        return {"metrics": {}, "metadata": {}, "parse_status": "empty_stdout"}

    try:
        parsed = json.loads(text)
    except json.JSONDecodeError as exc:
        return {
            "metrics": {},
            "metadata": {},
            "parse_status": "non_json_stdout",
            "parse_error": str(exc),
        }

    if not isinstance(parsed, dict):
        return {
            "metrics": {},
            "metadata": {},
            "parse_status": "json_not_object",
        }

    if "metrics" in parsed:
        metrics = parsed.get("metrics")
        return {
            "metrics": metrics if isinstance(metrics, dict) else {},
            "metadata": parsed.get("metadata", {}) if isinstance(parsed.get("metadata", {}), dict) else {},
            "raw": parsed.get("raw", {}),
            "parse_status": "ok",
        }

    return {
        "metrics": parsed,
        "metadata": {},
        "parse_status": "legacy_json_object",
    }


def _shell_command(command: list[str], env: dict[str, str]) -> str:
    env_assignments = [f"{key}={value}" for key, value in sorted(env.items())]
    return shlex.join([*env_assignments, *command])


def _scheduler_command(scheduler: dict[str, Any]) -> str:
    if scheduler.get("kind") != "scx":
        return ""
    env = scheduler.get("env", {})
    command = [scheduler["command"], *scheduler.get("args", [])]
    return _shell_command(command, env if isinstance(env, dict) else {})
