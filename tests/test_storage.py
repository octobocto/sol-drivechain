from configparser import ConfigParser
from contextlib import ExitStack
import gzip
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]
MOUNT = Path("/mnt/HC_Volume_106659003")
DATA = MOUNT / "sol-drivechain"
LOGROTATE = shutil.which("logrotate")


class ValidatorStorageTest(unittest.TestCase):
    def run_validator(self, **values):
        with tempfile.TemporaryDirectory() as path:
            root = Path(path)
            ledger = root / "ledger"
            keys = root / "keys"
            ledger.mkdir()
            keys.mkdir()
            result_file = root / "result.json"
            validator = root / "validator"
            validator.write_text(
                f"#!{sys.executable}\n"
                "import json\n"
                "import os\n"
                "import sys\n"
                "with open(os.environ['RESULT_FILE'], 'w') as file:\n"
                "    json.dump({'args': sys.argv[1:], "
                "'rust_log': os.environ['RUST_LOG']}, file)\n",
                encoding="utf-8",
            )
            keygen = root / "keygen"
            keygen.write_text(
                f"#!{sys.executable}\n"
                "import sys\n"
                "assert sys.argv[1] == 'pubkey'\n"
                "print('test-vote-key')\n",
                encoding="utf-8",
            )
            validator.chmod(0o755)
            keygen.chmod(0o755)
            env = {
                "PATH": os.defpath,
                "AGAVE_VALIDATOR": str(validator),
                "SOLANA_KEYGEN": str(keygen),
                "LEDGER": str(ledger),
                "KEYS": str(keys),
                "NOFILE": "256",
                "ENFORCER_URL": "http://127.0.0.1:50051",
                "SIDECHAIN_SLOT": "1",
                "RESULT_FILE": str(result_file),
                **values,
            }
            result = subprocess.run(
                ["bash", str(REPO / "genesis/run-validator.sh")],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            return json.loads(result_file.read_text(encoding="utf-8"))

    def assert_option(self, args, name, value):
        self.assertEqual(args.count(name), 1)
        self.assertEqual(args[args.index(name) + 1], value)

    def test_default_ledger_limit(self):
        result = self.run_validator()
        self.assert_option(result["args"], "--limit-ledger-size", "50000000")

    def test_custom_ledger_limit(self):
        result = self.run_validator(LEDGER_SHREDS="75000000")
        self.assert_option(result["args"], "--limit-ledger-size", "75000000")

    def test_bank_trace_disabled(self):
        result = self.run_validator()
        self.assertEqual(result["args"].count("--disable-banking-trace"), 1)

    def test_default_snapshot_limits(self):
        result = self.run_validator()
        self.assert_option(result["args"], "--full-snapshot-interval-slots", "25000")
        self.assert_option(result["args"], "--maximum-full-snapshots-to-retain", "10")

    def test_custom_snapshot_limits(self):
        result = self.run_validator(
            FULL_SNAPSHOT_INTERVAL_SLOTS="12000", FULL_SNAPSHOTS_TO_RETAIN="12"
        )
        self.assert_option(result["args"], "--full-snapshot-interval-slots", "12000")
        self.assert_option(result["args"], "--maximum-full-snapshots-to-retain", "12")

    def test_default_log_levels(self):
        result = self.run_validator()
        self.assertEqual(
            result["rust_log"],
            "solana=warn,sol_drivechain_bmm=info,solana_runtime::bank::bmm=info",
        )

    def test_custom_log_levels(self):
        result = self.run_validator(RUST_LOG="solana=error,sol_drivechain_bmm=debug")
        self.assertEqual(result["rust_log"], "solana=error,sol_drivechain_bmm=debug")


class StorageConfigTest(unittest.TestCase):
    def read_unit(self, name):
        unit = ConfigParser(strict=False, interpolation=None)
        unit.read(REPO / "genesis/systemd" / name)
        return unit

    def test_service_mount_guards(self):
        for name in (
            "sol-validator.service.d/storage.conf",
            "sol-regtest.service.d/storage.conf",
            "sol-logrotate.service",
        ):
            with self.subTest(unit=name):
                unit = self.read_unit(name)["Unit"]
                self.assertEqual(unit["RequiresMountsFor"], str(MOUNT))
                self.assertEqual(unit["AssertPathIsMountPoint"], str(MOUNT))

    def test_service_data_paths(self):
        for name, variable, path in (
            ("sol-validator", "LEDGER", DATA / "betanet/ledger"),
            ("sol-regtest", "ROOT", DATA / "regtest"),
        ):
            with self.subTest(service=name):
                unit_path = f"{name}.service.d/storage.conf"
                text = (REPO / "genesis/systemd" / unit_path).read_text()
                env = dict(
                    value.split("=", 1)
                    for line in text.splitlines()
                    if line.startswith("Environment=")
                    for value in shlex.split(line.split("=", 1)[1])
                )
                self.assertEqual(env[variable], str(path))
                self.assertEqual(env["LEDGER_SHREDS"], "50000000")
                service = self.read_unit(unit_path)["Service"]
                output = service["StandardOutput"]
                self.assertTrue(output.startswith("append:"))
                log = Path(output.split(":", 1)[1])
                self.assertEqual(log.parent, DATA / path.relative_to(DATA).parts[0])
                self.assertEqual(log.suffix, ".log")
                self.assertEqual(service["StandardError"], "inherit")

    def test_logrotate_state_path(self):
        service = self.read_unit("sol-logrotate.service")["Service"]
        args = shlex.split(service["ExecStart"])
        self.assertEqual(Path(args[0]).name, "logrotate")
        self.assertEqual(
            Path(args[args.index("--state") + 1]), DATA / "logrotate.status"
        )
        self.assertEqual(args[-1], "/etc/logrotate-sol-drivechain.conf")

    @unittest.skipIf(LOGROTATE is None, "The host has no logrotate tool.")
    def test_log_rotation_keeps_open_files_and_limits_archives(self):
        with tempfile.TemporaryDirectory() as path, ExitStack() as stack:
            root = Path(path)
            policy = (REPO / "genesis/logrotate/sol-drivechain.conf").read_text()
            config = root / "logrotate.conf"
            config.write_text(policy.replace(str(DATA), str(root)))
            logs = [
                root / name
                for name in (
                    "betanet/validator.log",
                    "regtest/service.log",
                    "regtest/validator.log",
                    "regtest/bitcoin-stack/enforcer/enforcer.log",
                    "regtest/bitcoin-stack/bitcoin/regtest/debug.log",
                )
            ]
            files = []
            for log in logs:
                log.parent.mkdir(parents=True, exist_ok=True)
                files.append(stack.enter_context(log.open("a")))
            for cycle in range(6):
                for file in files:
                    file.write(f"cycle {cycle}\n")
                    file.flush()
                result = subprocess.run(
                    [LOGROTATE, "--force", "--state", str(root / "state"), str(config)],
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for log in logs:
                    self.assertEqual(log.read_text(), "")
            for file, log in zip(files, logs):
                with self.subTest(log=log.relative_to(root)):
                    file.write("after\n")
                    file.flush()
                    self.assertEqual(log.read_text(), "after\n")
                    self.assertEqual(
                        sorted(item.name for item in log.parent.glob(f"{log.name}.*")),
                        [f"{log.name}.1"]
                        + [f"{log.name}.{count}.gz" for count in range(2, 6)],
                    )
                    self.assertEqual(Path(f"{log}.1").read_text(), "cycle 5\n")
                    for count in range(2, 6):
                        with gzip.open(f"{log}.{count}.gz", "rt") as archive:
                            self.assertEqual(archive.read(), f"cycle {6 - count}\n")


if __name__ == "__main__":
    unittest.main()
