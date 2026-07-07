#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(ROOT := Path(__file__).resolve().parents[2]))
from bench.config.parser import load_config


SRC = ROOT / "bench" / "workloads" / "src"
BIN = ROOT / "bench" / "workloads" / "bin"
BUILD = ROOT / "bench" / "workloads" / "build"


@dataclass(frozen=True)
class Workload:
    name: str
    repo: str
    ref: str


WORKLOADS = {
    "hackbench": Workload(
        name="hackbench",
        repo="https://github.com/linux-test-project/ltp.git",
        ref="master",
    ),
    "schbench": Workload(
        name="schbench",
        repo="https://kernel.googlesource.com/pub/scm/linux/kernel/git/mason/schbench",
        ref="master",
    ),
    "stress-ng": Workload(
        name="stress-ng",
        repo="https://github.com/ColinIanKing/stress-ng.git",
        ref="master",
    ),
    "fio": Workload(
        name="fio",
        repo="https://github.com/axboe/fio.git",
        ref="master",
    ),
    "redis": Workload(
        name="redis",
        repo="https://github.com/redis/redis.git",
        ref="unstable",
    ),
    "rt-tests": Workload(
        name="rt-tests",
        repo="https://git.kernel.org/pub/scm/utils/rt-tests/rt-tests.git",
        ref="main",
    ),
    "will-it-scale": Workload(
        name="will-it-scale",
        repo="https://github.com/antonblanchard/will-it-scale.git",
        ref="master",
    ),
    "perf": Workload(
        name="perf",
        repo="",
        ref="",
    ),
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Fetch and build benchmark workloads")
    parser.add_argument(
        "workloads",
        nargs="*",
        default=list(WORKLOADS),
        help=f"workloads to build: {', '.join(WORKLOADS)}",
    )
    parser.add_argument(
        "--config",
        default=str(ROOT / "bench" / "configs" / "local.config"),
        help="benchmark config path; used to find libvirt.kernel_source for perf",
    )
    parser.add_argument("--force", action="store_true", help="delete existing source before cloning")
    args = parser.parse_args(argv)
    config = load_config(args.config)

    SRC.mkdir(parents=True, exist_ok=True)
    BIN.mkdir(parents=True, exist_ok=True)
    BUILD.mkdir(parents=True, exist_ok=True)

    for name in args.workloads:
        if name not in WORKLOADS:
            raise SystemExit(f"unknown workload: {name}")
        workload = WORKLOADS[name]
        if name == "perf":
            build_perf(Path(config["libvirt"]["kernel_source"]))
            continue
        source = SRC / workload.name
        if args.force and source.exists():
            shutil.rmtree(source)
        clone_or_update(workload, source)
        build(workload.name, source)

    print(f"workload binaries: {BIN}")
    return 0


def clone_or_update(workload: Workload, source: Path) -> None:
    if not source.exists():
        run(["git", "clone", "--depth", "1", "--branch", workload.ref, workload.repo, str(source)])
        return

    run(["git", "fetch", "--depth", "1", "origin", workload.ref], cwd=source)
    run(["git", "checkout", "FETCH_HEAD"], cwd=source)


def build(name: str, source: Path) -> None:
    builders = {
        "hackbench": build_hackbench,
        "schbench": build_make_binary,
        "stress-ng": build_make_binary,
        "fio": build_fio,
        "redis": build_redis,
        "rt-tests": build_rt_tests,
        "will-it-scale": build_will_it_scale,
    }
    builders[name](source)


def build_hackbench(source: Path) -> None:
    candidates = list(source.rglob("hackbench.c"))
    if not candidates:
        raise RuntimeError("hackbench.c not found in LTP source")
    src = candidates[0]
    run(["gcc", "-O2", "-pthread", "-o", str(BIN / "hackbench"), str(src)])


def build_make_binary(source: Path) -> None:
    run(["make", f"-j{os.cpu_count() or 1}"], cwd=source)
    binary = source / source.name
    if not binary.exists():
        matches = list(source.glob(source.name.replace("-", "_")))
        if matches:
            binary = matches[0]
    if not binary.exists():
        binary = find_executable(source, source.name)
    install(binary, BIN / source.name)


def build_fio(source: Path) -> None:
    run(["./configure", "--disable-native"], cwd=source)
    run(["make", f"-j{os.cpu_count() or 1}"], cwd=source)
    install(source / "fio", BIN / "fio")


def build_redis(source: Path) -> None:
    run(["make", f"-j{os.cpu_count() or 1}", "BUILD_TLS=no"], cwd=source)
    install(source / "src" / "redis-server", BIN / "redis-server")
    install(source / "src" / "redis-benchmark", BIN / "redis-benchmark")


def build_rt_tests(source: Path) -> None:
    run(["make", f"-j{os.cpu_count() or 1}", "cyclictest"], cwd=source)
    install(find_executable(source, "cyclictest"), BIN / "cyclictest")


def build_will_it_scale(source: Path) -> None:
    run(["make", f"-j{os.cpu_count() or 1}"], cwd=source)
    if not (source / "runtest.py").exists():
        raise RuntimeError("will-it-scale runtest.py not found")
    wrapper = BIN / "will-it-scale"
    wrapper.write_text(
        "#!/bin/sh\n"
        f"cd {source.resolve()}\n"
        'exec ./runtest.py "$@"\n',
        encoding="utf-8",
    )
    wrapper.chmod(0o755)


def build_perf(kernel_source: Path) -> None:
    if not kernel_source:
        raise RuntimeError("kernel source path is required to build perf")
    perf_source = kernel_source / "tools" / "perf"
    if not perf_source.exists():
        raise RuntimeError(f"kernel source does not contain tools/perf: {kernel_source}")

    build_dir = BUILD / "perf"
    if build_dir.exists():
        shutil.rmtree(build_dir)
    build_dir.mkdir(parents=True)

    run(
        [
            "make",
            "-C",
            str(perf_source),
            f"O={build_dir}",
            f"-j{os.cpu_count() or 1}",
            "NO_LIBTRACEEVENT=1",
            "NO_LIBTRACEFS=1",
        ]
    )
    install(build_dir / "perf", BIN / "perf")


def find_executable(root: Path, name: str) -> Path:
    for path in root.rglob(name):
        if path.is_file() and os.access(path, os.X_OK):
            return path
    raise RuntimeError(f"built binary not found: {name}")


def install(src: Path, dst: Path) -> None:
    if not src.exists():
        raise RuntimeError(f"missing built binary: {src}")
    shutil.copy2(src, dst)
    dst.chmod(0o755)


def run(command: list[str], cwd: Path | None = None) -> None:
    print("+", " ".join(command), f"(cwd={cwd})" if cwd else "")
    subprocess.run(command, cwd=cwd, check=True)


if __name__ == "__main__":
    raise SystemExit(main())
