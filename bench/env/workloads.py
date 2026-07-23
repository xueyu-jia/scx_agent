#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.path.insert(0, str(ROOT := Path(__file__).resolve().parents[2]))
from bench.core.config import load_config


SRC = ROOT / "bench" / "workloads" / "src"
BIN = ROOT / "bench" / "workloads" / "bin"
BUILD = ROOT / "bench" / "workloads" / "build"


@dataclass(frozen=True)
class Workload:
    name: str
    repo: str
    ref: str
    source_type: str = "git"  # "git", "archive", or "local"
    archive_url: str = ""
    archive_extract_dir: str = ""  # subdirectory inside the archive
    local_path: str = ""


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
    "bpftool": Workload(
        name="bpftool",
        repo="",
        ref="",
    ),
    "ctx-clock": Workload(
        name="ctx-clock",
        repo="",
        ref="",
        source_type="archive",
        archive_url="http://phoronix-test-suite.com/benchmark-files/ctx_clock-1.zip",
        archive_extract_dir="ctx_clock",
    ),
    "osbench": Workload(
        name="osbench",
        repo="https://github.com/mbitsnbites/osbench.git",
        ref="master",
    ),
    "ipc-benchmark": Workload(
        name="ipc-benchmark",
        repo="",
        ref="",
        source_type="archive",
        archive_url="http://phoronix-test-suite.com/benchmark-files/ipc_benchmark-20200228.zip",
        archive_extract_dir="ipc_benchmark/ipc_benchmark-master",
    ),
    "sysbench": Workload(
        name="sysbench",
        repo="https://github.com/akopytov/sysbench.git",
        ref="master",
    ),
    "pmbench": Workload(
        name="pmbench",
        repo="",
        ref="",
        source_type="archive",
        archive_url="http://www.phoronix-test-suite.com/benchmark-files/jisooy-pmbench-46a3d394ca7b.tar.xz",
        archive_extract_dir="jisooy-pmbench-46a3d394ca7b",
    ),
    "mutex-benchmark": Workload(
        name="mutex-benchmark",
        repo="",
        ref="",
        source_type="archive",
        archive_url="http://phoronix-test-suite.com/benchmark-files/BenchmarkMutex-1.tar.xz",
        archive_extract_dir=".",
    ),
    "t-test1": Workload(
        name="t-test1",
        repo="",
        ref="",
        source_type="archive",
        archive_url="http://phoronix-test-suite.com/benchmark-files/t-test1c-20171.zip",
        archive_extract_dir="t-test1",
    ),
    "google-benchmark": Workload(
        name="google-benchmark",
        repo="https://github.com/google/benchmark.git",
        ref="main",
    ),
    "batch-microbench": Workload(
        name="batch-microbench",
        repo="",
        ref="",
        source_type="local",
        local_path="bench/workloads/local/batch_microbench",
    ),
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python3 -m bench.env workloads",
        description="Fetch and build benchmark workloads",
    )
    parser.add_argument(
        "workloads",
        nargs="*",
        default=list(WORKLOADS),
        help=f"workloads to build: {', '.join(WORKLOADS)}",
    )
    parser.add_argument(
        "--config",
        default=str(ROOT / "bench" / "configs" / "local_config"),
        help="benchmark config path; used to build perf and bpftool from kernel_source",
    )
    parser.add_argument("--force", action="store_true", help="delete existing source before cloning")
    args = parser.parse_args(argv)
    config = load_config(args.config)
    prepare_workloads(config, args.workloads, force=args.force)

    print(f"workload binaries: {BIN}")
    return 0


def prepare_workloads(
    config: dict[str, Any],
    workload_names: list[str],
    *,
    force: bool = False,
) -> None:
    SRC.mkdir(parents=True, exist_ok=True)
    BIN.mkdir(parents=True, exist_ok=True)
    BUILD.mkdir(parents=True, exist_ok=True)

    for name in workload_names:
        if name not in WORKLOADS:
            raise SystemExit(f"unknown workload: {name}")
        workload = WORKLOADS[name]
        if name == "perf":
            build_perf(Path(config["libvirt"]["kernel_source"]))
            continue
        if name == "bpftool":
            build_bpftool(Path(config["libvirt"]["kernel_source"]))
            continue
        source = (
            ROOT / workload.local_path
            if workload.source_type == "local"
            else SRC / workload.name
        )
        if force and source.exists() and workload.source_type != "local":
            shutil.rmtree(source)
        if workload.source_type == "local":
            if not source.exists():
                raise RuntimeError(f"local workload source does not exist: {source}")
        elif workload.source_type == "archive":
            fetch_archive(workload, source)
        else:
            clone_or_update(workload, source)
        build(workload.name, source)

def fetch_archive(workload: Workload, source: Path) -> None:
    url = workload.archive_url
    fname = url.rsplit("/", 1)[-1]
    archive = SRC / fname
    if not archive.exists():
        run(["curl", "-L", "-o", str(archive), url])
    source.mkdir(parents=True, exist_ok=True)
    if fname.endswith(".zip"):
        run(["unzip", "-o", str(archive), "-d", str(source)])
    elif fname.endswith(".tar.xz") or fname.endswith(".txz"):
        run(["tar", "-xf", str(archive), "-C", str(source)])
    elif fname.endswith(".tar.gz") or fname.endswith(".tgz"):
        run(["tar", "-xzf", str(archive), "-C", str(source)])


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
        "ctx-clock": build_ctx_clock,
        "osbench": build_osbench,
        "ipc-benchmark": build_ipc_benchmark,
        "sysbench": build_sysbench,
        "pmbench": build_pmbench,
        "mutex-benchmark": build_mutex_benchmark,
        "t-test1": build_t_test1,
        "google-benchmark": build_google_benchmark,
        "batch-microbench": build_batch_microbench,
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
            "NO_LIBLLVM=1",
        ]
    )
    install(build_dir / "perf", BIN / "perf")


def build_bpftool(kernel_source: Path) -> None:
    source = kernel_source / "tools" / "bpf" / "bpftool"
    if not source.exists():
        raise RuntimeError(f"kernel source does not contain tools/bpf/bpftool: {kernel_source}")

    build_dir = BUILD / "bpftool"
    if build_dir.exists():
        shutil.rmtree(build_dir)
    build_dir.mkdir(parents=True)
    llvm_strip = shutil.which("llvm-strip")
    if llvm_strip is None:
        llvm_config = shutil.which("llvm-config")
        if llvm_config is not None:
            bindir = subprocess.check_output(
                [llvm_config, "--bindir"], text=True
            ).strip()
            candidate = Path(bindir) / "llvm-strip"
            if candidate.is_file() and os.access(candidate, os.X_OK):
                llvm_strip = str(candidate)
    if llvm_strip is None:
        raise RuntimeError("building bpftool requires llvm-strip or llvm-config")
    run(
        [
            "make",
            "-C",
            str(source),
            f"OUTPUT={build_dir}/",
            f"LLVM_STRIP={llvm_strip}",
            "feature-llvm=0",
            f"-j{os.cpu_count() or 1}",
        ]
    )
    install(build_dir / "bpftool", BIN / "bpftool")


def find_executable(root: Path, name: str) -> Path:
    for path in root.rglob(name):
        if path.is_file() and os.access(path, os.X_OK):
            return path
    raise RuntimeError(f"built binary not found: {name}")


def build_ctx_clock(source: Path) -> None:
    src_file = source / "ctx_clock.c"
    if not src_file.exists():
        matches = list(source.rglob("ctx_clock.c"))
        if matches:
            src_file = matches[0]
    run(["gcc", "-O2", "-lpthread", "-o", str(BIN / "ctx_clock"), str(src_file)])


def build_osbench(source: Path) -> None:
    osbench_src = source / "src"
    run(["gcc", "-O2", "-I", str(osbench_src), "-c", str(osbench_src / "common" / "time.c"), "-o", str(BUILD / "osbench_time.o")])
    for name in ("create_threads", "create_processes", "launch_programs", "create_files", "mem_alloc"):
        libs = ["-lpthread"] if name == "create_threads" else []
        libs.append("-lm") if name == "create_files" else None
        run(["gcc", "-O2", "-I", str(osbench_src), "-o", str(BIN / name), str(osbench_src / f"{name}.c"), str(BUILD / "osbench_time.o"), *libs])


def build_ipc_benchmark(source: Path) -> None:
    ipc_dir = source / "ipc_benchmark" / "ipc_benchmark-master"
    if not ipc_dir.exists():
        matches = list(source.rglob("Makefile"))
        if matches:
            ipc_dir = matches[0].parent
    for target in ("pipe", "fifo", "socketpair", "tcp"):
        run(["make", target], cwd=ipc_dir)
        install(ipc_dir / target, BIN / target)


def build_sysbench(source: Path) -> None:
    run(["./autogen.sh"], cwd=source)
    run(["./configure", "--without-mysql"], cwd=source)
    run(["make", f"-j{os.cpu_count() or 1}"], cwd=source)
    install(source / "src" / "sysbench", BIN / "sysbench")


def build_pmbench(source: Path) -> None:
    pmbench_dir = source / "jisooy-pmbench-46a3d394ca7b"
    if not pmbench_dir.exists():
        matches = list(source.rglob("Makefile"))
        if matches:
            pmbench_dir = matches[0].parent
    run(["make", "pmbench"], cwd=pmbench_dir)
    install(pmbench_dir / "pmbench", BIN / "pmbench")


def build_mutex_benchmark(source: Path) -> None:
    benchmark_dir = SRC / "google-benchmark"
    if not benchmark_dir.exists():
        raise RuntimeError(
            "google-benchmark must be fetched first: "
            "python3 -m bench.env workloads google-benchmark"
        )
    build_dir = benchmark_dir / "build"
    if not (build_dir / "src" / "libbenchmark.a").exists():
        build_dir.mkdir(parents=True, exist_ok=True)
        run(["cmake", "-DCMAKE_BUILD_TYPE=Release", "-DBENCHMARK_ENABLE_TESTING=OFF", ".."], cwd=build_dir)
        run(["make", f"-j{os.cpu_count() or 1}"], cwd=build_dir)
    cpp_file = source / "BenchmarkMutex.cpp"
    if not cpp_file.exists():
        matches = list(source.rglob("BenchmarkMutex.cpp"))
        if matches:
            cpp_file = matches[0]
    run([
        "g++", "-std=c++17", "-O2",
        "-I", str(benchmark_dir / "include"),
        "-o", str(BIN / "BenchmarkMutex"),
        str(cpp_file),
        "-L", str(build_dir / "src"),
        "-lbenchmark", "-pthread",
    ])


def build_google_benchmark(source: Path) -> None:
    build_dir = source / "build"
    build_dir.mkdir(parents=True, exist_ok=True)
    run(["cmake", "-DCMAKE_BUILD_TYPE=Release", "-DBENCHMARK_ENABLE_TESTING=OFF", ".."], cwd=build_dir)
    run(["make", f"-j{os.cpu_count() or 1}"], cwd=build_dir)


def build_t_test1(source: Path) -> None:
    src_file = source / "t-test1.c"
    if not src_file.exists():
        matches = list(source.rglob("t-test1.c"))
        if matches:
            src_file = matches[0]
    run(["gcc", "-O2", "-pthread", "-o", str(BIN / "t-test1"), str(src_file)])


def build_batch_microbench(source: Path) -> None:
    run(["make", f"-j{os.cpu_count() or 1}"], cwd=source)
    install(source / "batch_microbench", BIN / "batch_microbench")


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
