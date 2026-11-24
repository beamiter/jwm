#!/usr/bin/env python3
"""Offline CI dependency contract: no privileged commands, network, or installs."""
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts/ci-install-deps.sh"
WORKFLOWS = ("ci.yml", "remote-wayland-safety.yml", "remote-window-safety.yml",
             "remote-popup-qt.yml")


class DependencyInstallTests(unittest.TestCase):
    ARCHIVES = "/var/cache/apt/archives/jwm-bridge-ci.aB0dc9E7"

    def invoke(self, packages=("libx11-dev", "pkg-config"), update=0, install=0, source="valid",
               bridge=False, cjk=False, download=(0, 0), archives=ARCHIVES, cleanup=0):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            log = folder / "calls.jsonl"
            sources = folder / "ubuntu.sources"
            if source != "missing":
                sources.write_text("Types: deb\nURIs: https://archive.ubuntu.com/ubuntu\nSuites: noble\nComponents: main\n" if source == "valid" else "")
            fake = folder / "sudo"
            fake.write_text(textwrap.dedent("""\
                #!/usr/bin/python3
                import json, os, sys
                args = sys.argv[1:]
                with open(os.environ['FAKE_APT_LOG'], 'a') as log:
                    log.write(json.dumps(args) + '\\n')
                command = args[1] if args[0] == '-n' else args[0]
                if command == '/usr/bin/mktemp':
                    print(os.environ['FAKE_ARCHIVES'])
                    sys.exit(0)
                if command == '/usr/bin/chmod':
                    sys.exit(0)
                if command == '/usr/bin/rm':
                    sys.exit(int(os.environ['FAKE_CLEANUP_STATUS']))
                if command == '/usr/bin/timeout':
                    assert '--download-only' in args
                    index = ['120s', '360s'].index(args[3])
                    sys.exit(json.loads(os.environ['FAKE_DOWNLOAD_STATUS'])[index])
                assert command == 'apt-get', args
                phase = 'UPDATE' if args[-1] == 'update' else 'INSTALL'
                sys.exit(int(os.environ['FAKE_' + phase + '_STATUS']))
            """))
            fake.chmod(0o755)
            env = dict(os.environ, PATH=str(folder), FAKE_APT_LOG=str(log),
                       JWM_CI_APT_SOURCES=str(sources), FAKE_UPDATE_STATUS=str(update),
                       FAKE_INSTALL_STATUS=str(install), FAKE_DOWNLOAD_STATUS=json.dumps(download),
                       FAKE_ARCHIVES=archives, FAKE_CLEANUP_STATUS=str(cleanup),
                       JWM_CI_BRIDGE_DOWNLOAD_RETRY=str(int(bridge)),
                       JWM_CI_PREFETCH_CJK=str(int(cjk)))
            result = subprocess.run(["/bin/bash", str(HELPER), *packages], env=env,
                                    text=True, capture_output=True, timeout=5, check=False)
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            # Normalize only the temporary fixture path in recorded argv.
            calls = [[arg.replace(str(sources), "<ubuntu.sources>") for arg in call] for call in calls]
            return result, calls

    def test_success_uses_strict_update_and_bounded_official_source_commands(self):
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        options = ["apt-get", "-o", "Dir::Etc::sourcelist=<ubuntu.sources>",
                   "-o", "Dir::Etc::sourceparts=-", "-o", "Acquire::http::Timeout=30", "-o",
                   "Acquire::https::Timeout=30", "-o", "Acquire::Retries=2",
                   "-o", "DPkg::Lock::Timeout=60"]
        self.assertEqual(calls[0], options + ["-o", "APT::Update::Error-Mode=any", "update"])
        self.assertEqual(calls[1], options + ["install", "-y", "--no-install-recommends",
                                            "libx11-dev", "pkg-config"])

    def test_update_failure_is_preserved_and_install_is_not_attempted(self):
        result, calls = self.invoke(update=100)
        self.assertEqual(result.returncode, 100)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][-1], "update")

    def test_install_failure_is_preserved_without_outer_retry(self):
        result, calls = self.invoke(install=100)
        self.assertEqual(result.returncode, 100)
        self.assertEqual(len(calls), 2)

    def test_empty_package_list_fails_before_any_command(self):
        result, calls = self.invoke(packages=())
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])

    def test_missing_or_empty_sources_fail_before_any_privileged_command(self):
        for source in ("missing", "empty"):
            with self.subTest(source=source):
                result, calls = self.invoke(source=source)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(calls, [])
                self.assertIn("missing or empty Ubuntu source file", result.stderr)

    def test_bridge_cache_is_minted_by_root_and_all_phases_use_same_apt_config(self):
        result, calls = self.invoke(bridge=True, packages=("dbus", "mpv", "ffmpeg"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 6)
        self.assertEqual(calls[0][-3:], ["-o", "APT::Update::Error-Mode=any", "update"])
        self.assertEqual(calls[1], ["-n", "/usr/bin/mktemp", "-d", "--",
                                    "/var/cache/apt/archives/jwm-bridge-ci.XXXXXXXX"])
        self.assertEqual(calls[2], ["-n", "/usr/bin/chmod", "0755", "--", self.ARCHIVES])
        apt = calls[0][:-3] + ["-o", f"Dir::Cache::archives={self.ARCHIVES}"]
        packages = ["install", "-y", "--no-install-recommends", "dbus", "mpv", "ffmpeg"]
        self.assertEqual(calls[3], ["-n", "/usr/bin/timeout", "--kill-after=5s", "120s"]
                         + apt + ["--download-only"] + packages)
        self.assertEqual(calls[4], ["-n"] + apt + ["--no-download"] + packages)
        self.assertEqual(calls[5], ["-n", "/usr/bin/rm", "-rf", "--", self.ARCHIVES])

    def test_bridge_reissues_only_download_on_timeout_and_reuses_archive(self):
        result, calls = self.invoke(bridge=True, download=(124, 0))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 7)
        self.assertEqual(calls[3][3], "120s")
        self.assertEqual(calls[4][3], "360s")
        self.assertEqual(calls[3][4:], calls[4][4:])
        self.assertIn("--no-download", calls[5])
        self.assertIn("retrying once from the same sources", result.stderr)

    def test_bridge_skips_install_on_failure_and_preserves_status(self):
        cases = (((100, 0), 1, 100), ((137, 0), 1, 137), ((124, 124), 2, 124),
                 ((124, 100), 2, 100))
        for download, attempts, status in cases:
            with self.subTest(download=download):
                result, calls = self.invoke(bridge=True, download=download)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual([c[3] for c in calls if "--download-only" in c],
                                 ["120s", "360s"][:attempts])
                self.assertFalse(any("--no-download" in c for c in calls))
                self.assertEqual(calls[-1], ["-n", "/usr/bin/rm", "-rf", "--", self.ARCHIVES])

    def test_bridge_never_retries_install_or_ignores_cleanup_failure(self):
        for install, expected in ((100, 100), (0, 1)):
            with self.subTest(install=install):
                result, calls = self.invoke(bridge=True, install=install, cleanup=1)
                self.assertEqual(result.returncode, expected)
                self.assertEqual(sum("--no-download" in c for c in calls), 1)
                self.assertIn("could not remove private Bridge archives", result.stderr)

    def test_bridge_bad_archive_path_or_failed_update_never_downloads_or_deletes(self):
        result, calls = self.invoke(bridge=True, archives="/tmp/caller-controlled")
        self.assertEqual(result.returncode, 1)
        self.assertIn("unexpected private archive path", result.stderr)
        self.assertEqual(len(calls), 2)  # update and creation only
        self.assertEqual(calls[-1][1], "/usr/bin/mktemp")
        result, calls = self.invoke(bridge=True, update=100)
        self.assertEqual(result.returncode, 100)
        self.assertEqual(len(calls), 1)

    def test_cjk_and_bridge_modes_fail_before_sudo_if_combined(self):
        result, calls = self.invoke(bridge=True, cjk=True)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])
        self.assertIn("cannot be combined", result.stderr)

    def test_all_workflow_installs_have_step_deadlines_and_no_bypass(self):
        count = 0
        bridge_count = 0
        for name in WORKFLOWS:
            text = (ROOT / ".github/workflows" / name).read_text()
            self.assertNotIn("apt-get", text, name)
            steps = re.split(r"(?m)^[ \t]*-[ \t]+(?=name:|uses:)", text)
            installs = [step for step in steps if "scripts/ci-install-deps.sh" in step]
            self.assertTrue(installs, name)
            for step in installs:
                self.assertRegex(step, r"(?m)^[ \t]+timeout-minutes:[ \t]+10[ \t]*$")
                self.assertNotIn("continue-on-error", step)
                self.assertNotIn("|| true", step)
                self.assertNotIn("cargo test", step)  # install timeout must not truncate tests
                if "JWM_CI_BRIDGE_DOWNLOAD_RETRY" in step:
                    self.assertIn("Install D-Bus and playback test tools", step)
                    self.assertIn("ci-install-deps.sh dbus mpv ffmpeg", step)
                    bridge_count += 1
                count += 1
        self.assertEqual(count, 14)
        self.assertEqual(bridge_count, 1)


class AptSourceSelectionTests(unittest.TestCase):
    """Exercise real apt parsing with synthetic indexes, no network or install."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.folder = Path(self.directory.name)
        self.lists = self.folder / "lists"
        self.parts = self.folder / "sources.list.d"
        self.lists.mkdir()
        self.parts.mkdir()
        self.sources = self.folder / "ubuntu.sources"
        self.sources.write_text(
            "Types: deb\nURIs: http://official.invalid/ubuntu\n"
            "Suites: noble\nComponents: main\nArchitectures: amd64\n"
        )
        (self.parts / "vendor.list").write_text(
            "deb [arch=amd64] http://vendor.invalid/ubuntu noble main\n"
        )
        (self.folder / "status").write_text("")
        # Do not read machine-specific APT configuration or its installed state.
        config = self.folder / "apt.conf"
        config.write_text('Dir::Etc::parts "-";\nDir::Etc::main "-";\n')
        self.env = dict(os.environ, APT_CONFIG=str(config), LC_ALL="C")
        self.options = [
            "-o", f"Dir::Etc::sourcelist={self.sources}",
            "-o", "Dir::Etc::sourceparts=-",
            "-o", f"Dir::State::lists={self.lists}",
            "-o", f"Dir::State::status={self.folder / 'status'}",
            "-o", "Dir::Cache::pkgcache=", "-o", "Dir::Cache::srcpkgcache=",
            "-o", "APT::Architecture=amd64",
        ]

    def run_apt(self, program, *args, vendor=False):
        options = self.options.copy()
        if vendor:
            options += ["-o", f"Dir::Etc::sourceparts={self.parts}"]
        result = subprocess.run([program, *options, *args], env=self.env,
                                capture_output=True, text=True, timeout=10, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_print_uris_excludes_vendor_without_changing_source_files(self):
        before = {path: path.read_bytes() for path in (self.sources, self.parts / "vendor.list")}
        all_sources = self.run_apt("apt-get", "--print-uris", "update", vendor=True)
        self.assertIn("http://official.invalid/ubuntu/", all_sources)
        self.assertIn("http://vendor.invalid/ubuntu/", all_sources)
        selected = self.run_apt("apt-get", "--print-uris", "update")
        self.assertIn("http://official.invalid/ubuntu/", selected)
        self.assertNotIn("vendor.invalid", selected)
        for path, content in before.items():
            self.assertEqual(path.read_bytes(), content)

    def test_cached_vendor_index_is_not_an_install_candidate(self):
        for host, version in (("official", "1"), ("vendor", "2")):
            index = self.lists / f"{host}.invalid_ubuntu_dists_noble_main_binary-amd64_Packages"
            index.write_text(
                f"Package: jwm-ci-source-probe\nVersion: {version}\n"
                "Architecture: amd64\nDescription: offline fixture\n\n"
            )
        all_sources = self.run_apt("apt-cache", "policy", "jwm-ci-source-probe", vendor=True)
        self.assertIn("Candidate: 2", all_sources)
        self.assertIn("vendor.invalid", all_sources)
        selected = self.run_apt("apt-cache", "policy", "jwm-ci-source-probe")
        self.assertIn("Candidate: 1", selected)
        self.assertNotIn("vendor.invalid", selected)
        self.assertIn("official.invalid", selected)


class QtProbeContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = (ROOT / ".github/workflows/remote-popup-qt.yml").read_text()
        lines = cls.workflow.splitlines()
        blocks = []
        for index, line in enumerate(lines):
            match = re.match(r"^([ \t]*)run:[ \t]*\|[ \t]*$", line)
            if not match:
                continue
            indent = len(match.group(1))
            body = []
            for following in lines[index + 1:]:
                if following.strip() and len(following) - len(following.lstrip()) <= indent:
                    break
                body.append(following)
            script = textwrap.dedent("\n".join(body))
            if "test_name=" in script and "--ignored" in script:
                blocks.append(script)
        if len(blocks) != 1:
            raise AssertionError("Qt workflow must have one mandatory exact-test block")
        cls.script = blocks[0]

    def run_contract(self, listed=1, status=0,
                     summary="test result: ok. 1 passed; 0 failed; 0 ignored;", list_status=0):
        # Execute only the workflow's control/validation shell with fake cargo.
        # No compilation, display server, native app, or network is started.
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            binary = folder / "bin"
            binary.mkdir()
            log = folder / "calls.jsonl"
            cargo = binary / "cargo"
            cargo.write_text(
                "#!/usr/bin/python3\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['QT_FAKE_CALLS'], 'a') as f:\n"
                "    f.write(json.dumps({'args': args, 'display': os.getenv('DISPLAY'), "
                "'wayland': os.getenv('WAYLAND_DISPLAY')}) + '\\n')\n"
                "if '--list' in args:\n"
                "    for i in range(int(os.environ['QT_FAKE_LIST'])): print('fixture_' + str(i) + ': test')\n"
                "    sys.exit(int(os.environ['QT_FAKE_LIST_STATUS']))\n"
                "print('PRIVATE_SYNTHETIC_WIRE_LOG_DO_NOT_PUBLISH')\n"
                "print('JWM_QT_STAGE=root_mapped')\n"
                "print('JWM_QT_STAGE=PRIVATE_SYNTHETIC_UNKNOWN_STAGE')\n"
                "print(os.environ['QT_FAKE_SUMMARY'])\n"
                "sys.exit(int(os.environ['QT_FAKE_STATUS']))\n"
            )
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=str(binary) + ":/usr/bin:/bin", RUNNER_TEMP=str(folder),
                       QT_FAKE_CALLS=str(log), QT_FAKE_LIST=str(listed),
                       QT_FAKE_LIST_STATUS=str(list_status), QT_FAKE_STATUS=str(status),
                       QT_FAKE_SUMMARY=summary, DISPLAY="must_be_unset", WAYLAND_DISPLAY="must_be_unset")
            result = subprocess.run(["/bin/bash", "-c", self.script], env=env, text=True,
                                    capture_output=True, timeout=5, check=False)
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            self.assertNotIn("PRIVATE_SYNTHETIC", result.stdout + result.stderr)
            return result, calls

    def test_native_wayland_and_mandatory_software_gl_contract(self):
        for key, value in (("EGL_PLATFORM", "surfaceless"), ("LIBGL_ALWAYS_SOFTWARE", "1"),
                           ("JWM_REQUIRE_HEADLESS_GL", "1")):
            self.assertRegex(self.workflow, rf"(?m)^[ \t]*{key}:[ \t]*['\"]?{value}['\"]?[ \t]*$")
        fixture = (ROOT / "src/backend/wayland_udev/remote_popup_qt_tests.rs").read_text()
        self.assertRegex(fixture, r'\.env\(\s*"QT_QPA_PLATFORM"\s*,\s*"wayland"\s*\)')
        self.assertRegex(fixture, r'HeadlessSmithayGles::new\(\)\.expect\(')
        self.assertNotIn("upload-artifact", self.workflow)
        self.assertNotIn("continue-on-error", self.workflow)

    def test_exact_ignored_test_runs_once_and_raw_output_stays_private(self):
        result, calls = self.run_contract()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        exact_name = "backend::wayland_udev::remote_popup::qt_tests::actual_qt_dynamic_private_seat_menu_selection_and_terminal_retirement"
        for call in calls:
            self.assertIn(exact_name, call["args"])
            self.assertIn("--ignored", call["args"])
            self.assertIn("--exact", call["args"])
            self.assertIn("remote-wayland,backend-wayland-udev", call["args"])
        self.assertIn("--list", calls[0]["args"])
        self.assertNotIn("--list", calls[1]["args"])
        self.assertIn("--test-threads=1", calls[1]["args"])
        self.assertIsNone(calls[1]["display"])
        self.assertIsNone(calls[1]["wayland"])
        self.assertIn("qt_stage=root_mapped", result.stdout)
        self.assertIn("qt_exact_test_passed=true", result.stdout)

    def test_missing_duplicate_or_failed_listing_never_executes_test(self):
        for listed, status in ((0, 0), (2, 0), (1, 7)):
            with self.subTest(listed=listed, status=status):
                result, calls = self.run_contract(listed=listed, list_status=status)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(calls), 1)

    def test_nonzero_execution_fails_even_with_success_summary(self):
        result, calls = self.run_contract(status=9)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 2)

    def test_zero_exit_requires_one_pass_and_zero_ignored(self):
        for summary in ("test result: ok. 0 passed; 0 failed; 0 ignored;",
                        "test result: ok. 1 passed; 0 failed; 1 ignored;", ""):
            with self.subTest(summary=summary):
                result, calls = self.run_contract(summary=summary)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(calls), 2)


if __name__ == "__main__":
    unittest.main()
