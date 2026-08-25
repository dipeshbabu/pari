from __future__ import annotations

import subprocess
import sys
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: install_wheel.py DIST_DIR")
    wheels = sorted(Path(sys.argv[1]).glob("*.whl"))
    if len(wheels) != 1:
        raise SystemExit(f"expected exactly one wheel, found {len(wheels)}: {wheels}")
    subprocess.check_call(
        [sys.executable, "-m", "pip", "install", "--force-reinstall", str(wheels[0])]
    )


if __name__ == "__main__":
    main()
