"""Real upgrade transitions; refusal of an existing workflow is a failure."""

import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time

from baseline import sha256


def install(home, binary, firmware):
    """Keep old mappings intact: replacement is by rename, never truncation."""
    for source, destination in ((binary, home / "bin/msb"), (firmware, home / "lib" / firmware.name)):
        destination.parent.mkdir(parents=True, exist_ok=True)
        staged = destination.with_name(destination.name + ".staged")
        shutil.copy2(source, staged)
        if destination.name == "msb":
            staged.chmod(0o700)
        os.replace(staged, destination)
    for name in ("libkrunfw.so", "libkrunfw.so." + firmware.name.split('.')[2]):
        link = home / "lib" / name
        staged = link.with_name(link.name + ".staged")
        staged.symlink_to(firmware.name)
        os.replace(staged, link)


def execute(*, commands, manifest, payload, runtimes, scripts, output, image,
            environment, run, schema, cleanup, lifecycle_command, validate_report):
    cases = []
    # Running upgrade is independent of SDK provisioning. The following cases
    # require both old and new SDKs to operate on CLI-prepared, populated homes.
    selections = [("running-upgrade", None), *[(f"after-cli-upgrade-{sdk}", sdk) for sdk in commands]]
    old, old_fw = runtimes["released"]
    new, new_fw = runtimes["candidate"]
    for label, sdk in selections:
        root = Path(tempfile.mkdtemp(prefix="msb-upgrade-", dir="/tmp"))
        home = root / "home"
        home.mkdir()
        (home / "config.json").write_text("{}\n")
        case = dict(case=label, home=str(home), status="running", steps=[], failures=[])
        cases.append(case)
        started = time.monotonic()
        cli = home / "bin/msb"
        env = environment(home, cli, home / "lib" / old_fw.name, True)
        log_path = output / f"{label}.log"
        evidence = output / f"{label}-runtime.jsonl"
        env.update(MSB_COMPAT_PYTHON=sys.executable,
                   MSB_COMPAT_VERIFY_RUNTIME=str(scripts / "verify_runtime.py"),
                   MSB_COMPAT_RUNTIME_REPORT=str(evidence), MSB_COMPAT_RUNTIME_SHA256=sha256(old))
        try:
            with log_path.open('w') as log:
                def step(name, command, selected_env=None, required=True):
                    before = time.monotonic()
                    item = dict(step=name)
                    case["steps"].append(item)
                    try:
                        run(command, selected_env or env, home, log, 180)
                        item["status"] = "passed"
                    except Exception as error:
                        item.update(status="failed", error=repr(error))
                        case["failures"].append(f"{name}: {error}")
                        if required:
                            raise
                    finally:
                        item["seconds"] = round(time.monotonic()-before, 3)

                def verify(name):
                    step("verify-runtime-" + name, [sys.executable, scripts / "verify_runtime.py", name])
                    return json.loads(evidence.read_text().splitlines()[-1])

                def seed(name):
                    step("create-" + name, [cli, "create", image, "--name", name, "--memory", "256M",
                         "--cpus", "1", "--max-duration", "10m", "--env", "COMPAT_MARKER=retained",
                         "--tmpfs", "/compat-data-0:8M"])
                    verify(name)
                    step("write-" + name, [cli, "exec", name, "--", "sh", "-ec", "printf retained > /root/compat-marker"])

                install(home, old, old_fw)
                seed("retained")
                step("stop-retained", [cli, "stop", "retained"])
                old_schema = schema(home)
                if sdk is None:
                    seed("active")
                    old_identity = verify("active")
                install(home, new, new_fw)
                env = environment(home, cli, home / "lib" / new_fw.name, True) | {
                    "MSB_COMPAT_PYTHON": sys.executable, "MSB_COMPAT_VERIFY_RUNTIME": str(scripts / "verify_runtime.py"),
                    "MSB_COMPAT_RUNTIME_REPORT": str(evidence), "MSB_COMPAT_RUNTIME_SHA256": sha256(new)}
                if sdk is None:
                    # Diagnose each control path even when starting another VM
                    # fails. Do not turn the stop-all restriction into a pass.
                    step("new-cli-ps", [cli, "ps", "--format", "json"], required=False)
                    step("new-cli-exec-old", [cli, "exec", "active", "--", "cat", "/root/compat-marker"], required=False)
                    step("start-with-old-vm-running", [cli, "start", "retained"], required=False)
                    old_env = dict(env, MSB_COMPAT_RUNTIME_SHA256=sha256(old),
                                   MSB_COMPAT_RUNTIME_IDENTITIES=json.dumps({"active": sha256(old), "retained": sha256(new)}))
                    step("old-vm-still-same-runtime", [sys.executable, scripts / "verify_runtime.py", "active"], old_env)
                    identity = json.loads(evidence.read_text().splitlines()[-1])
                    if {r['pid'] for r in identity['runtimes']} != {r['pid'] for r in old_identity['runtimes']}:
                        raise AssertionError("upgrade replaced the old running VM")
                    if schema(home) != old_schema:
                        raise AssertionError("catalog schema changed while old VM was still running")
                    step("stop-old-with-new-cli", [cli, "stop", "active"])
                    step("stop-target-before-retry", [cli, "stop", "retained"])
                step("new-cli-start-retained", [cli, "start", "retained"])
                verify("retained")
                step("new-cli-retained-data", [cli, "exec", "retained", "--", "sh", "-ec",
                     'test "$(cat /root/compat-marker)" = retained; test "$COMPAT_MARKER" = retained; test "$(stat -f -c %T /compat-data-0)" = tmpfs'])
                step("new-cli-stop-retained", [cli, "stop", "retained"])
                case["schema_before_cli"] = old_schema
                case["schema_after_cli"] = schema(home)
                if sdk is not None:
                    command, sdk_env, cwd = commands[sdk]
                    command = lifecycle_command(manifest["language"], command, payload / sdk, scripts)
                    sdk_report = output / f"{label}-sdk.json"
                    sdk_evidence = output / f"{label}-sdk-runtime.jsonl"
                    sdk_env = env | sdk_env | {
                        "MSB_COMPAT_EXISTING": "retained", "MSB_COMPAT_IMAGE": image,
                        "MSB_COMPAT_REPORT": str(sdk_report), "MSB_COMPAT_RUNTIME_REPORT": str(sdk_evidence),
                        "MSB_COMPAT_SDK_VERSION": manifest["sdks"][sdk]["version"],
                        "MSB_COMPAT_SDK_ROOT": str(payload / sdk), "MSB_COMPAT_CLI": str(cli)}
                    before = schema(home)
                    # Node resolves its real installed package beside the fixture;
                    # Rust/Go binaries retain their prepared dependency provenance.
                    run(command, sdk_env, cwd, log)
                    validate_report(json.loads(sdk_report.read_text()), sdk_evidence.read_text().splitlines(), sha256(new))
                    if schema(home) != before:
                        raise AssertionError("SDK changed the CLI-prepared catalog schema")
                    case["steps"].append(dict(step="sdk-existing-record-restart", status="passed"))
                if case["failures"]:
                    raise AssertionError("; ".join(case["failures"]))
                case["status"] = "passed"
        except Exception as error:
            case.update(status="failed", error=repr(error))
        finally:
            try:
                with (output / f"{label}-cleanup.log").open('w') as log:
                    cleanup(home, [cli, new], env, log)
            except Exception as error:
                case.update(status="failed", cleanup_error=repr(error))
            else:
                if case["status"] == "passed":
                    shutil.rmtree(root)
            case["seconds"] = round(time.monotonic()-started, 3)
            (output / f"{label}.json").write_text(json.dumps(case, indent=2) + "\n")
            print(json.dumps({k:v for k,v in case.items() if not k.startswith('schema_')}), flush=True)
    return cases
