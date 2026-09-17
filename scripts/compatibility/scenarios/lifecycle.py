"""Common public lifecycle API, including access to a pre-existing catalog row."""
import asyncio
import json
import os
from pathlib import Path
import subprocess

from microsandbox import Sandbox, Volume


async def main():
    existing = os.environ.get("MSB_COMPAT_EXISTING")
    report = {"status": "failed", "passed": []}
    try:
        for count in ([1] if existing else [0, 1, 3]):
            name = existing or f"compat-common-{count}"
            sandbox = await Sandbox.start(name) if existing else await Sandbox.create(
                name, image=os.environ["MSB_COMPAT_IMAGE"], memory=256, cpus=1,
                env={"COMPAT_MARKER": "retained"},
                volumes={f"/compat-data-{i}": Volume.tmpfs(size_mib=8) for i in range(count)})
            for restart in range(2):
                subprocess.run([os.environ["MSB_COMPAT_PYTHON"], os.environ["MSB_COMPAT_VERIFY_RUNTIME"], name], check=True)
                script = 'test "$COMPAT_MARKER" = retained; '
                if not existing and not restart:
                    script += 'printf retained > /root/compat-marker; '
                script += 'test "$(cat /root/compat-marker)" = retained; '
                for i in range(count):
                    script += f'test "$(stat -f -c %T /compat-data-{i})" = tmpfs; '
                result = await sandbox.exec('sh', ['-ec', script])
                assert result.success, result.stderr_text
                await sandbox.stop()
                if not restart:
                    sandbox = await Sandbox.start(name)
            report["passed"].append(f"{name}/runtime-env-mounts-disk-restart")
            if not existing:
                await Sandbox.remove(name)
        report["status"] = "passed"
    except BaseException as error:
        report["error"] = str(error)
        raise
    finally:
        Path(os.environ["MSB_COMPAT_REPORT"]).write_text(json.dumps(report))


if __name__ == '__main__':
    asyncio.run(main())
