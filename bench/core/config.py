from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError as exc:  # pragma: no cover - environment guard
    raise SystemExit("PyYAML is required: install the 'yaml' Python package") from exc


REQUIRED_TOP_LEVEL_KEYS = (
    "libvirt",
    "schedulers",
    "plans",
    "machines",
    "suites",
    "metric_profiles",
    "benches",
)

VALID_DIRECTIONS = {"higher", "lower"}
VALID_CHARTS = {"delta_bar", "latency_bar", "summary_table"}
VALID_SCHEDULER_KINDS = {"builtin", "scx"}
VALID_BUILTIN_SCHEDULER_KEYS = {"kind"}
VALID_SCX_SCHEDULER_KEYS = {
    "kind",
    "command",
    "host_command",
    "host_kconfig",
    "host_support_files",
    "args",
    "env",
    "settle_seconds",
}
VALID_MACHINE_KEYS = {"vcpus", "memory", "pin_cpus", "exclusive", "frequency"}
VALID_BENCH_DEFAULT_KEYS = {"post_warmup_settle_seconds", "cooldown_seconds"}
VALID_BENCH_KEYS = {
    "measurement",
    "warmup",
    "host_support_files",
    "post_warmup_settle_seconds",
    "cooldown_seconds",
    "env",
}
VALID_COMMAND_KEYS = {"command", "args", "timeout_seconds"}
VALID_TREATMENT_KEYS = {
    *VALID_COMMAND_KEYS,
    "host_command",
    "host_support_files",
    "env",
    "post_treatment_settle_seconds",
}
RESERVED_GUEST_ENV = {
    "SCX_BENCH_OUT",
    "SCX_BENCH_ROLE",
    "SCX_BENCH_VARIANT",
    "SCX_BENCH_TREATMENT",
    "SCX_BENCH_TREATMENT_OUTCOME",
    "SCX_BENCH_WORKDIR",
}
VALID_EXECUTOR_KEYS = {
    "parallel",
    "cpu_source",
    "isolated_cpus",
    "irq_cpus",
    "smt_policy",
    "pair_policy",
    "memory_guard_gb",
}
VALID_LIBVIRT_KEYS = {
    "uri",
    "kernel",
    "kernel_args",
    "kernel_source",
    "initrd",
    "root_image",
    "runtime_dir",
    "network",
    "ssh_user",
    "ssh_key",
    "ssh_host",
    "ssh_port",
    "workdir",
    "guest_output_dir",
    "emulator_cpus",
    "iothread_cpus",
    "pin_vhost_threads",
    "timeout_extra_seconds",
    "boot_timeout_seconds",
    "vm_settle_seconds",
    "destroy_on_exit",
    "cpu_mode",
}


class ConfigError(ValueError):
    """Raised when the benchmark config is invalid."""


@dataclass(frozen=True)
class RunSpec:
    plan: str
    run_index: int
    machine_name: str
    suite_name: str
    bench_name: str
    metric_profile_name: str
    machine: dict[str, Any]
    suite: dict[str, Any]
    bench: dict[str, Any]
    metric_profile: dict[str, Any]
    libvirt: dict[str, Any]
    executor: dict[str, Any]


def load_config(path: str | Path) -> dict[str, Any]:
    config_path = Path(path)
    with config_path.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f)

    if data is None:
        raise ConfigError(f"{config_path} is empty")
    if not isinstance(data, dict):
        raise ConfigError(f"{config_path} must contain a mapping at the top level")

    validate_config(data)
    return data


def validate_config(config: dict[str, Any]) -> None:
    for key in REQUIRED_TOP_LEVEL_KEYS:
        if key not in config:
            raise ConfigError(f"missing top-level key: {key}")
        if not isinstance(config[key], dict):
            raise ConfigError(f"{key} must be a mapping")

    _validate_libvirt(config)
    _validate_schedulers(config)
    _validate_treatments(config)
    _validate_executor(config)
    _validate_plans(config)
    _validate_machines(config)
    _validate_suites(config)
    _validate_metric_profiles(config)
    _validate_bench_defaults(config)
    _validate_benches(config)


def expand_plan(config: dict[str, Any], plan_name: str) -> list[RunSpec]:
    plans = config["plans"]
    if plan_name not in plans:
        raise ConfigError(f"unknown plan: {plan_name}")

    plan = plans[plan_name]
    runs = plan.get("runs", 1)
    specs: list[RunSpec] = []

    for run_index in range(1, runs + 1):
        for matrix_entry in plan["matrix"]:
            machine_name = matrix_entry["machine"]
            machine = config["machines"][machine_name]

            for suite_name in matrix_entry["suites"]:
                suite = config["suites"][suite_name]
                metric_profile_name = suite["metric_profile"]
                metric_profile = config["metric_profiles"][metric_profile_name]

                for bench_name in suite["benches"]:
                    specs.append(
                        RunSpec(
                            plan=plan_name,
                            run_index=run_index,
                            machine_name=machine_name,
                            suite_name=suite_name,
                            bench_name=bench_name,
                            metric_profile_name=metric_profile_name,
                            machine=machine,
                            suite=suite,
                            bench=_bench_with_defaults(config, bench_name),
                            metric_profile=metric_profile,
                            libvirt=config["libvirt"],
                            executor=config.get("executor", {}),
                        )
                    )

    return specs


def _validate_libvirt(config: dict[str, Any]) -> None:
    libvirt = config["libvirt"]
    unknown_keys = set(libvirt) - VALID_LIBVIRT_KEYS
    if unknown_keys:
        raise ConfigError(f"libvirt has unsupported keys: {sorted(unknown_keys)}")

    uri = libvirt.get("uri", "qemu:///system")
    if not isinstance(uri, str):
        raise ConfigError("libvirt.uri must be a string")

    kernel = libvirt.get("kernel")
    if kernel is not None:
        if not isinstance(kernel, str):
            raise ConfigError("libvirt.kernel must be a string")
        if not Path(kernel).exists():
            raise ConfigError(f"libvirt.kernel does not exist: {kernel}")

    kernel_args = libvirt.get("kernel_args", "")
    if not isinstance(kernel_args, str):
        raise ConfigError("libvirt.kernel_args must be a string")

    initrd = libvirt.get("initrd")
    if initrd is not None:
        if not isinstance(initrd, str):
            raise ConfigError("libvirt.initrd must be a string")
        if not Path(initrd).exists():
            raise ConfigError(f"libvirt.initrd does not exist: {initrd}")

    root_image = libvirt.get("root_image")
    if not isinstance(root_image, str):
        raise ConfigError("libvirt.root_image must be a string")

    runtime_dir = libvirt.get("runtime_dir")
    if runtime_dir is not None and not isinstance(runtime_dir, str):
        raise ConfigError("libvirt.runtime_dir must be a string")

    kernel_source = libvirt.get("kernel_source")
    if not isinstance(kernel_source, str):
        raise ConfigError("libvirt.kernel_source must be a string")
    if not Path(kernel_source).exists():
        raise ConfigError(f"libvirt.kernel_source does not exist: {kernel_source}")
    if not (Path(kernel_source) / "tools" / "perf").exists():
        raise ConfigError(f"libvirt.kernel_source does not contain tools/perf: {kernel_source}")

    network = libvirt.get("network", "default")
    if network is not None and not isinstance(network, str):
        raise ConfigError("libvirt.network must be a string or null")

    for key in ("ssh_user", "ssh_key", "workdir", "guest_output_dir", "emulator_cpus"):
        value = libvirt.get(key)
        if not isinstance(value, str):
            raise ConfigError(f"libvirt.{key} must be a string")

    iothread_cpus = libvirt.get("iothread_cpus")
    if iothread_cpus is not None:
        if not isinstance(iothread_cpus, str):
            raise ConfigError("libvirt.iothread_cpus must be a string or null")
        parse_cpu_list(iothread_cpus)

    pin_vhost_threads = libvirt.get("pin_vhost_threads", False)
    if not isinstance(pin_vhost_threads, bool):
        raise ConfigError("libvirt.pin_vhost_threads must be a boolean")

    ssh_host = libvirt.get("ssh_host")
    if ssh_host is not None and not isinstance(ssh_host, str):
        raise ConfigError("libvirt.ssh_host must be a string")

    ssh_port = libvirt.get("ssh_port", 22)
    if not isinstance(ssh_port, int) or ssh_port < 1:
        raise ConfigError("libvirt.ssh_port must be a positive integer")

    parse_cpu_list(libvirt["emulator_cpus"])

    timeout_extra = libvirt.get("timeout_extra_seconds", 120)
    if not isinstance(timeout_extra, int) or timeout_extra < 0:
        raise ConfigError("libvirt.timeout_extra_seconds must be a non-negative integer")

    boot_timeout = libvirt.get("boot_timeout_seconds", 10)
    if not isinstance(boot_timeout, int) or boot_timeout < 1:
        raise ConfigError("libvirt.boot_timeout_seconds must be a positive integer")

    vm_settle = libvirt.get("vm_settle_seconds", 0)
    if isinstance(vm_settle, bool) or not isinstance(vm_settle, int) or vm_settle < 0:
        raise ConfigError("libvirt.vm_settle_seconds must be a non-negative integer")

    destroy_on_exit = libvirt.get("destroy_on_exit", True)
    if not isinstance(destroy_on_exit, bool):
        raise ConfigError("libvirt.destroy_on_exit must be a boolean")

    cpu_mode = libvirt.get("cpu_mode", "host-passthrough")
    if not isinstance(cpu_mode, str):
        raise ConfigError("libvirt.cpu_mode must be a string")


def _validate_schedulers(config: dict[str, Any]) -> None:
    for scheduler_name, scheduler in config["schedulers"].items():
        if not isinstance(scheduler, dict):
            raise ConfigError(f"schedulers.{scheduler_name} must be a mapping")
        kind = scheduler.get("kind")
        if kind not in VALID_SCHEDULER_KINDS:
            raise ConfigError(
                f"schedulers.{scheduler_name}.kind must be one of {sorted(VALID_SCHEDULER_KINDS)}"
            )

        valid_keys = (
            VALID_SCX_SCHEDULER_KEYS
            if kind == "scx"
            else VALID_BUILTIN_SCHEDULER_KEYS
        )
        unknown_keys = set(scheduler) - valid_keys
        if unknown_keys:
            raise ConfigError(
                f"schedulers.{scheduler_name} has unsupported keys: "
                f"{sorted(unknown_keys)}"
            )

        if kind == "scx":
            command = scheduler.get("command")
            if not isinstance(command, str) or not command:
                raise ConfigError(
                    f"schedulers.{scheduler_name}.command must be a non-empty string"
                )
            host_command = scheduler.get("host_command")
            if host_command is not None and (
                not isinstance(host_command, str) or not host_command
            ):
                raise ConfigError(
                    f"schedulers.{scheduler_name}.host_command must be a non-empty string"
                )
            host_kconfig = scheduler.get("host_kconfig")
            if host_kconfig is not None and (
                not isinstance(host_kconfig, str) or not host_kconfig
            ):
                raise ConfigError(
                    f"schedulers.{scheduler_name}.host_kconfig must be a non-empty string"
                )
            if host_kconfig is not None and host_command is None:
                raise ConfigError(
                    f"schedulers.{scheduler_name}.host_kconfig requires host_command"
                )
            host_support_files = scheduler.get("host_support_files", [])
            if not isinstance(host_support_files, list) or any(
                not isinstance(item, str) or not item for item in host_support_files
            ):
                raise ConfigError(
                    f"schedulers.{scheduler_name}.host_support_files must be a string list"
                )
            if host_support_files and host_command is None:
                raise ConfigError(
                    f"schedulers.{scheduler_name}.host_support_files requires host_command"
                )
            args = scheduler.get("args", [])
            if not isinstance(args, list):
                raise ConfigError(f"schedulers.{scheduler_name}.args must be a list")
            for arg in args:
                if not isinstance(arg, str) or not arg:
                    raise ConfigError(
                        f"schedulers.{scheduler_name}.args entries must be non-empty strings"
                    )
        env = scheduler.get("env", {})
        if not isinstance(env, dict):
            raise ConfigError(f"schedulers.{scheduler_name}.env must be a mapping")
        for key, value in env.items():
            if not isinstance(key, str) or not isinstance(value, str):
                raise ConfigError(f"schedulers.{scheduler_name}.env entries must be string:string")
        _reject_reserved_environment(env, f"schedulers.{scheduler_name}.env")

        settle = scheduler.get("settle_seconds", 0)
        if isinstance(settle, bool) or not isinstance(settle, int) or settle < 0:
            raise ConfigError(
                f"schedulers.{scheduler_name}.settle_seconds must be a non-negative integer"
            )


def _validate_treatments(config: dict[str, Any]) -> None:
    treatments = config.get("treatments", {})
    if not isinstance(treatments, dict):
        raise ConfigError("treatments must be a mapping")

    for treatment_name, treatment in treatments.items():
        prefix = f"treatments.{treatment_name}"
        if not isinstance(treatment_name, str) or not treatment_name:
            raise ConfigError("treatment names must be non-empty strings")
        if not isinstance(treatment, dict):
            raise ConfigError(f"{prefix} must be a mapping")

        unknown_keys = set(treatment) - VALID_TREATMENT_KEYS
        if unknown_keys:
            raise ConfigError(f"{prefix} has unsupported keys: {sorted(unknown_keys)}")
        _validate_command(treatment, prefix, allowed_keys=VALID_TREATMENT_KEYS)
        host_command = treatment.get("host_command")
        if host_command is not None and (
            not isinstance(host_command, str) or not host_command
        ):
            raise ConfigError(f"{prefix}.host_command must be a non-empty string")
        host_support_files = treatment.get("host_support_files", [])
        if not isinstance(host_support_files, list) or any(
            not isinstance(item, str) or not item for item in host_support_files
        ):
            raise ConfigError(f"{prefix}.host_support_files must be a string list")
        _validate_non_negative_seconds(
            treatment,
            prefix,
            ("post_treatment_settle_seconds",),
        )

        env = treatment.get("env", {})
        if not isinstance(env, dict):
            raise ConfigError(f"{prefix}.env must be a mapping")
        for key, value in env.items():
            if not isinstance(key, str) or not isinstance(value, str):
                raise ConfigError(f"{prefix}.env entries must be string:string")
        _reject_reserved_environment(env, f"{prefix}.env")


def _validate_executor(config: dict[str, Any]) -> None:
    executor = config.get("executor", {})
    if not isinstance(executor, dict):
        raise ConfigError("executor must be a mapping")

    unknown_keys = set(executor) - VALID_EXECUTOR_KEYS
    if unknown_keys:
        raise ConfigError(f"executor has unsupported keys: {sorted(unknown_keys)}")

    parallel = executor.get("parallel", 1)
    if parallel != "auto" and (not isinstance(parallel, int) or parallel < 1):
        raise ConfigError("executor.parallel must be 'auto' or a positive integer")

    cpu_source = executor.get("cpu_source", "configured")
    if cpu_source not in ("configured", "isolated"):
        raise ConfigError("executor.cpu_source must be 'configured' or 'isolated'")

    isolated_cpus = executor.get("isolated_cpus")
    if isolated_cpus is not None:
        if not isinstance(isolated_cpus, str):
            raise ConfigError("executor.isolated_cpus must be a string")
        parse_cpu_list(isolated_cpus)

    irq_cpus = executor.get("irq_cpus")
    if irq_cpus is not None:
        if not isinstance(irq_cpus, str):
            raise ConfigError("executor.irq_cpus must be a string or null")
        parse_cpu_list(irq_cpus)

    smt_policy = executor.get("smt_policy", "use_all_siblings")
    if smt_policy != "use_all_siblings":
        raise ConfigError("executor.smt_policy must be 'use_all_siblings'")

    pair_policy = executor.get("pair_policy", "sequential")
    if pair_policy != "sequential":
        raise ConfigError("executor.pair_policy must be 'sequential'")

    memory_guard = executor.get("memory_guard_gb", 0)
    if not isinstance(memory_guard, int) or memory_guard < 0:
        raise ConfigError("executor.memory_guard_gb must be a non-negative integer")


def _validate_plans(config: dict[str, Any]) -> None:
    for plan_name, plan in config["plans"].items():
        if not isinstance(plan, dict):
            raise ConfigError(f"plans.{plan_name} must be a mapping")

        runs = plan.get("runs", 1)
        if not isinstance(runs, int) or runs < 1:
            raise ConfigError(f"plans.{plan_name}.runs must be a positive integer")

        matrix = plan.get("matrix")
        if not isinstance(matrix, list) or not matrix:
            raise ConfigError(f"plans.{plan_name}.matrix must be a non-empty list")

        for index, entry in enumerate(matrix):
            prefix = f"plans.{plan_name}.matrix[{index}]"
            if not isinstance(entry, dict):
                raise ConfigError(f"{prefix} must be a mapping")

            machine = entry.get("machine")
            if not isinstance(machine, str):
                raise ConfigError(f"{prefix}.machine must be a string")
            if machine not in config["machines"]:
                raise ConfigError(f"{prefix}.machine references unknown machine: {machine}")

            suites = entry.get("suites")
            if not isinstance(suites, list) or not suites:
                raise ConfigError(f"{prefix}.suites must be a non-empty list")
            for suite in suites:
                if not isinstance(suite, str):
                    raise ConfigError(f"{prefix}.suites entries must be strings")
                if suite not in config["suites"]:
                    raise ConfigError(f"{prefix}.suites references unknown suite: {suite}")


def _validate_machines(config: dict[str, Any]) -> None:
    for machine_name, machine in config["machines"].items():
        if not isinstance(machine, dict):
            raise ConfigError(f"machines.{machine_name} must be a mapping")
        unknown_keys = set(machine) - VALID_MACHINE_KEYS
        if unknown_keys:
            raise ConfigError(
                f"machines.{machine_name} has unsupported keys: {sorted(unknown_keys)}"
            )
        if "vcpus" not in machine:
            raise ConfigError(f"machines.{machine_name}.vcpus is required")
        if "memory" not in machine:
            raise ConfigError(f"machines.{machine_name}.memory is required")
        if not isinstance(machine["vcpus"], int) or machine["vcpus"] < 1:
            raise ConfigError(f"machines.{machine_name}.vcpus must be a positive integer")
        if not isinstance(machine["memory"], str):
            raise ConfigError(f"machines.{machine_name}.memory must be a string")
        pin_cpus_value = machine.get("pin_cpus")
        if not isinstance(pin_cpus_value, str):
            raise ConfigError(f"machines.{machine_name}.pin_cpus must be a string")
        if pin_cpus_value != "auto":
            pin_cpus = parse_cpu_list(pin_cpus_value)
            if len(pin_cpus) != machine["vcpus"]:
                raise ConfigError(
                    f"machines.{machine_name}.pin_cpus must contain exactly "
                    f"{machine['vcpus']} CPU(s)"
                )
        if machine.get("exclusive") is not True:
            raise ConfigError(f"machines.{machine_name}.exclusive must be true")
        frequency = machine.get("frequency")
        if not isinstance(frequency, dict):
            raise ConfigError(f"machines.{machine_name}.frequency must be a mapping")
        if frequency.get("fixed") is not True:
            raise ConfigError(f"machines.{machine_name}.frequency.fixed must be true")
        governor = frequency.get("governor")
        if governor is not None and (not isinstance(governor, str) or not governor):
            raise ConfigError(
                f"machines.{machine_name}.frequency.governor must be a non-empty string"
            )
        target_khz = frequency.get("target_khz")
        if target_khz is not None and (
            not isinstance(target_khz, int) or isinstance(target_khz, bool) or target_khz <= 0
        ):
            raise ConfigError(
                f"machines.{machine_name}.frequency.target_khz must be a positive integer"
            )
        turbo = frequency.get("turbo")
        if turbo is not None and not isinstance(turbo, bool):
            raise ConfigError(f"machines.{machine_name}.frequency.turbo must be a boolean")


def _validate_suites(config: dict[str, Any]) -> None:
    for suite_name, suite in config["suites"].items():
        if not isinstance(suite, dict):
            raise ConfigError(f"suites.{suite_name} must be a mapping")

        benches = suite.get("benches")
        if not isinstance(benches, list) or not benches:
            raise ConfigError(f"suites.{suite_name}.benches must be a non-empty list")
        for bench in benches:
            if not isinstance(bench, str):
                raise ConfigError(f"suites.{suite_name}.benches entries must be strings")
            if bench not in config["benches"]:
                raise ConfigError(f"suites.{suite_name}.benches references unknown bench: {bench}")

        metric_profile = suite.get("metric_profile")
        if not isinstance(metric_profile, str):
            raise ConfigError(f"suites.{suite_name}.metric_profile must be a string")
        if metric_profile not in config["metric_profiles"]:
            raise ConfigError(
                f"suites.{suite_name}.metric_profile references unknown profile: {metric_profile}"
            )


def _validate_metric_profiles(config: dict[str, Any]) -> None:
    for profile_name, profile in config["metric_profiles"].items():
        if not isinstance(profile, dict):
            raise ConfigError(f"metric_profiles.{profile_name} must be a mapping")

        primary = profile.get("primary")
        if not isinstance(primary, list) or not primary:
            raise ConfigError(f"metric_profiles.{profile_name}.primary must be a non-empty list")

        for index, metric in enumerate(primary):
            prefix = f"metric_profiles.{profile_name}.primary[{index}]"
            if not isinstance(metric, dict):
                raise ConfigError(f"{prefix} must be a mapping")
            if not isinstance(metric.get("name"), str):
                raise ConfigError(f"{prefix}.name must be a string")
            if metric.get("direction") not in VALID_DIRECTIONS:
                raise ConfigError(f"{prefix}.direction must be one of {sorted(VALID_DIRECTIONS)}")
            if "regression" in metric and not isinstance(metric["regression"], str):
                raise ConfigError(f"{prefix}.regression must be a string")
            if "unit" in metric and not isinstance(metric["unit"], str):
                raise ConfigError(f"{prefix}.unit must be a string")
            if "chart" in metric and metric["chart"] not in VALID_CHARTS:
                raise ConfigError(f"{prefix}.chart must be one of {sorted(VALID_CHARTS)}")

        secondary = profile.get("secondary", [])
        if not isinstance(secondary, list):
            raise ConfigError(f"metric_profiles.{profile_name}.secondary must be a list")
        for metric in secondary:
            if not isinstance(metric, str):
                raise ConfigError(f"metric_profiles.{profile_name}.secondary entries must be strings")


def _validate_bench_defaults(config: dict[str, Any]) -> None:
    defaults = config.get("bench_defaults", {})
    if not isinstance(defaults, dict):
        raise ConfigError("bench_defaults must be a mapping")
    unknown_keys = set(defaults) - VALID_BENCH_DEFAULT_KEYS
    if unknown_keys:
        raise ConfigError(f"bench_defaults has unsupported keys: {sorted(unknown_keys)}")
    _validate_non_negative_seconds(defaults, "bench_defaults", VALID_BENCH_DEFAULT_KEYS)


def _validate_benches(config: dict[str, Any]) -> None:
    for bench_name, bench in config["benches"].items():
        prefix = f"benches.{bench_name}"
        if not isinstance(bench, dict):
            raise ConfigError(f"{prefix} must be a mapping")
        unknown_keys = set(bench) - VALID_BENCH_KEYS
        if unknown_keys:
            raise ConfigError(f"{prefix} has unsupported keys: {sorted(unknown_keys)}")
        if "measurement" not in bench:
            raise ConfigError(f"{prefix}.measurement is required")
        _validate_command(bench["measurement"], f"{prefix}.measurement")
        if "warmup" in bench:
            _validate_command(bench["warmup"], f"{prefix}.warmup")

        host_support_files = bench.get("host_support_files", [])
        if not isinstance(host_support_files, list) or any(
            not isinstance(item, str) or not item for item in host_support_files
        ):
            raise ConfigError(f"{prefix}.host_support_files must be a string list")

        _validate_non_negative_seconds(
            bench,
            prefix,
            ("post_warmup_settle_seconds", "cooldown_seconds"),
        )

        env = bench.get("env", {})
        if not isinstance(env, dict):
            raise ConfigError(f"{prefix}.env must be a mapping")
        for key, value in env.items():
            if not isinstance(key, str) or not isinstance(value, str):
                raise ConfigError(f"{prefix}.env entries must be string:string")
        _reject_reserved_environment(env, f"{prefix}.env")


def _bench_with_defaults(config: dict[str, Any], bench_name: str) -> dict[str, Any]:
    return {
        **config.get("bench_defaults", {}),
        **config["benches"][bench_name],
    }


def _validate_command(
    value: Any,
    prefix: str,
    *,
    allowed_keys: set[str] = VALID_COMMAND_KEYS,
) -> None:
    if not isinstance(value, dict):
        raise ConfigError(f"{prefix} must be a mapping")

    unknown_keys = set(value) - allowed_keys
    if unknown_keys:
        raise ConfigError(f"{prefix} has unsupported keys: {sorted(unknown_keys)}")

    for key in ("command", "timeout_seconds"):
        if key not in value:
            raise ConfigError(f"{prefix}.{key} is required")

    if not isinstance(value["command"], str) or not value["command"]:
        raise ConfigError(f"{prefix}.command must be a non-empty string")

    args = value.get("args", [])
    if not isinstance(args, list):
        raise ConfigError(f"{prefix}.args must be a list")
    for arg in args:
        if not isinstance(arg, str) or not arg:
            raise ConfigError(f"{prefix}.args entries must be non-empty strings")

    timeout = value["timeout_seconds"]
    if isinstance(timeout, bool) or not isinstance(timeout, int) or timeout < 1:
        raise ConfigError(f"{prefix}.timeout_seconds must be a positive integer")


def _reject_reserved_environment(env: dict[str, Any], prefix: str) -> None:
    reserved = sorted(set(env) & RESERVED_GUEST_ENV)
    if reserved:
        raise ConfigError(f"{prefix} must not override reserved variables: {reserved}")


def _validate_non_negative_seconds(
    values: dict[str, Any],
    prefix: str,
    keys: tuple[str, ...] | set[str],
) -> None:
    for key in keys:
        value = values.get(key, 0)
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise ConfigError(f"{prefix}.{key} must be a non-negative integer")


def parse_cpu_list(value: str) -> list[int]:
    cpus: list[int] = []
    for part in value.split(","):
        part = part.strip()
        if not part:
            raise ConfigError(f"invalid CPU list: {value}")
        if "-" in part:
            start_text, end_text = part.split("-", 1)
            if not start_text.isdigit() or not end_text.isdigit():
                raise ConfigError(f"invalid CPU range: {part}")
            start = int(start_text)
            end = int(end_text)
            if start > end:
                raise ConfigError(f"invalid CPU range: {part}")
            cpus.extend(range(start, end + 1))
        else:
            if not part.isdigit():
                raise ConfigError(f"invalid CPU id: {part}")
            cpus.append(int(part))

    if len(cpus) != len(set(cpus)):
        raise ConfigError(f"duplicate CPU id in list: {value}")
    return cpus
