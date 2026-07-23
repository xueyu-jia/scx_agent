from __future__ import annotations

import sys

from bench.env import isolation, manager, workloads


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if not args or args[0] in {"-h", "--help"}:
        print(
            "usage: python3 -m bench.env "
            "{init,verify,rebuild-image,restore,workloads,isolation} ...\n\n"
            "Manage the benchmark host and guest environment.\n\n"
            "commands:\n"
            "  init           generate config and prepare the environment\n"
            "  verify         verify the prepared environment\n"
            "  rebuild-image  rebuild the guest base image\n"
            "  restore        restore managed host settings\n"
            "  workloads      fetch and build workload binaries\n"
            "  isolation      manage CPU, IRQ, and frequency isolation"
        )
        return 0
    if args and args[0] == "workloads":
        return workloads.main(args[1:])
    if args and args[0] == "isolation":
        return isolation.main(args[1:])
    return manager.main(args)


if __name__ == "__main__":
    raise SystemExit(main())
