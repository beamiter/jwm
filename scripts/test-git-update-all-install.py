#!/usr/bin/env python3
"""Exercise update/build/install routing with local Git remotes and stub tools."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import re

SCRIPT = Path(__file__).resolve().with_name('git-update-all.sh')


class UpdateInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='git-update-install-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.projects = self.root / 'projects'
        self.projects.mkdir()
        self.tools = self.root / 'tools'
        self.tools.mkdir()
        self.calls = self.root / 'calls'
        self.installed = self.root / 'installed'
        self.installed.mkdir()
        self.env = {**os.environ, 'PATH': str(self.tools) + ':/usr/bin:/bin',
                    'TEST_CALLS': str(self.calls), 'GIT_CONFIG_NOSYSTEM': '1',
                    'GIT_CONFIG_GLOBAL': '/dev/null'}
        for name in ('jsh', 'anvil', 'ember', 'forge', 'frost'):
            self.env[name.upper() + '_INSTALL_ARGS'] = '--bin-dir ' + str(self.installed)
        for tool in ('cargo', 'nix', 'sudo'):
            self.executable(self.tools / tool,
                            '#!/bin/sh\nprintf "%s:%s\\n" "' + tool + '" "$*" >> "$TEST_CALLS"\nexit 0\n')

    @staticmethod
    def executable(path, source):
        path.write_text(source)
        path.chmod(0o755)

    def git(self, *args):
        return subprocess.run(['git', *map(str, args)], env=self.env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)

    def project(self, name, updated=False, fail=False):
        seed = self.root / ('seed-' + name)
        self.git('init', '-q', '-b', 'main', seed)
        self.git('-C', seed, 'config', 'user.email', 'test@example.invalid')
        self.git('-C', seed, 'config', 'user.name', 'Fixture')
        (seed / '.gitignore').write_text('/target/\n')
        scripts = seed / 'scripts'
        scripts.mkdir()
        installer = 'install-jsh.sh' if name == 'jsh' else 'install-jwm.sh' if name == 'jwm' else 'install.sh'
        self.executable(scripts / installer,
                        '#!/bin/sh\nprintf "installer:%s\\n" "' + name + '" >> "$TEST_CALLS"\n'
                        'if [ "${TEST_RECORD_ARGS:-}" = 1 ]; then printf "args:%s:%s\\n" "' + name + '" "$*" >> "$TEST_CALLS"; fi\n'
                        'exit ' + ('9' if fail else '0') + '\n')
        self.git('-C', seed, 'add', '.')
        self.git('-C', seed, 'commit', '-qm', 'fixture')
        remote = self.root / (name + '.git')
        self.git('clone', '-q', '--bare', seed, remote)
        checkout = self.projects / name
        self.git('clone', '-q', remote, checkout)
        artifact = checkout / 'target' / 'release' / name
        artifact.parent.mkdir(parents=True)
        self.executable(artifact, '#!/bin/sh\nexit 0\n')
        if updated:
            (seed / 'changed').write_text('new source\n')
            self.git('-C', seed, 'add', '.')
            self.git('-C', seed, 'commit', '-qm', 'new version')
            self.git('-C', seed, 'push', '-q', remote, 'main')
        return checkout

    def run_script(self, *args):
        return subprocess.run(['bash', str(SCRIPT), *args, str(self.projects)],
                              env=self.env, text=True, capture_output=True, timeout=20)

    def lines(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_default_installs_each_known_project_through_its_own_script(self):
        names = ('jsh', 'anvil', 'ember', 'frost', 'forge', 'jwm')
        for name in names:
            self.project(name)
        result = self.run_script('-B', '-q', '-J', '2')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertCountEqual([line for line in self.lines() if line.startswith('installer:')],
                              ['installer:' + name for name in names])
        self.assertFalse(any(line.startswith(('cargo:', 'nix:')) for line in self.lines()))
        self.assertIn('已安装', result.stdout)

    def test_updated_project_is_installed_without_force_option(self):
        self.project('ember', updated=True)
        result = self.run_script('-q')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), ['installer:ember'])
        self.assertIn('已拉取', result.stdout)

    def test_build_only_keeps_installers_out_of_the_execution_path(self):
        self.project('ember')
        result = self.run_script('-B', '-C', '-q')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), ['cargo:build --release --locked'])
        self.assertIn('已编译', result.stdout)

    def test_jwm_build_only_builds_bridge_and_player_helpers(self):
        self.project('jwm')
        result = self.run_script('-B', '-C', '-q')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), ['cargo:build --release --locked -p jwm -p jwm-bridge --bins'])

    def test_jwm_readiness_requires_player_beside_compositor(self):
        # Exercise the production predicate against a private install prefix;
        # never create or inspect a fixture in the host's /usr/local/bin.
        predicate = re.search(r'^jwm_helpers_are_ready\(\) \{\n.*?^\}',
                              SCRIPT.read_text(), re.MULTILINE | re.DOTALL).group()
        for name in ('jwm', 'jwm-bridge'):
            self.executable(self.installed / name, '#!/bin/sh\nexit 0\n')
        def ready():
            return subprocess.run(['bash', '-c', predicate + '\njwm_helpers_are_ready "$1"',
                                   'test', str(self.installed / 'jwm')],
                                  env=self.env, capture_output=True).returncode
        self.assertNotEqual(ready(), 0)
        self.executable(self.installed / 'jwm-player', '#!/bin/sh\nexit 0\n')
        self.assertEqual(ready(), 0)
        (self.installed / 'jwm-player').chmod(0o644)
        self.assertNotEqual(ready(), 0)

    def test_missing_installed_command_triggers_install_even_with_existing_artifact(self):
        self.project('ember')
        result = self.run_script('-q')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), ['installer:ember'])

    def test_unchanged_installed_project_is_skipped(self):
        self.project('ember')
        self.executable(self.installed / 'ember', '#!/bin/sh\nexit 0\n')
        result = self.run_script('-q')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), [])

    def test_update_only_and_dry_run_never_build_or_install(self):
        self.project('ember', updated=True)
        for options in (('-B', '-n', '-q'), ('-B', '-N', '-q')):
            result = self.run_script(*options)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(self.lines(), [])

    def test_skip_build_keeps_git_updates_and_only_target_scope(self):
        checkouts = {name: self.project(name, updated=True)
                     for name in ('ember', 'frost', 'jwm')}
        before = {name: self.git('-C', path, 'rev-parse', 'HEAD').stdout.strip()
                  for name, path in checkouts.items()}
        result = self.run_script('-B', '-q', '-T', 'ember,frost', '-X', 'ember,jwm')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), ['installer:frost'])
        for name in ('ember', 'frost'):
            self.assertTrue((checkouts[name] / 'changed').exists(), name)
            self.assertNotEqual(self.git('-C', checkouts[name], 'rev-parse', 'HEAD').stdout.strip(),
                                before[name], name)
        self.assertFalse((checkouts['jwm'] / 'changed').exists())
        self.assertEqual(self.git('-C', checkouts['jwm'], 'rev-parse', 'HEAD').stdout.strip(),
                         before['jwm'])

    def test_skip_build_overrides_missing_artifacts_and_force_build(self):
        checkout = self.project('ember')
        (checkout / 'target' / 'release' / 'ember').unlink()
        self.assertFalse((self.installed / 'ember').exists())
        for options in (('-q', '-X', 'ember'), ('-B', '-q', '-X', 'ember')):
            with self.subTest(options=options):
                result = self.run_script(*options)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.lines(), [])

    def test_skip_build_never_requests_jwm_sudo_credentials(self):
        checkout = self.project('jwm', updated=True)
        result = self.run_script('-B', '-q', '--skip-build=jwm')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((checkout / 'changed').exists())
        self.assertEqual(self.lines(), [])

    def test_skip_build_never_runs_jsh_toolchain_preflight(self):
        self.project('jsh')
        self.env['JSH_INSTALL_TARGET'] = 'x86_64-unknown-linux-musl'
        self.executable(self.tools / 'x86_64-linux-musl-gcc', '#!/bin/sh\nexit 0\n')
        self.executable(self.tools / 'rustup',
                        '#!/bin/sh\nprintf "rustup:%s\\n" "$*" >> "$TEST_CALLS"\n'
                        'printf "x86_64-unknown-linux-musl\\n"\n')
        result = self.run_script('-B', '-C', '-q', '--skip-build', 'jsh')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), [])

    def local_runner(self, status=0):
        runner = self.projects / 'jterm_core' / 'scripts' / 'family-cargo.py'
        runner.parent.mkdir(parents=True)
        runner.write_text(
            'import os, sys\n'
            'with open(os.environ["TEST_CALLS"], "a") as log: log.write("local:" + " ".join(sys.argv[1:]) + "\\n")\n'
            f'sys.exit({status})\n')

    def test_local_family_build_precedes_install_of_the_exact_built_binary(self):
        checkout = self.project('ember')
        self.local_runner()
        self.env['TEST_RECORD_ARGS'] = '1'
        result = self.run_script('-B', '-L', '-q')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.lines(), ['local:ember build --release', 'installer:ember',
                                      'args:ember:--binary ' + str(checkout / 'target/release/ember') + ' --bin-dir ' + str(self.installed)])

    def test_local_build_failure_never_runs_the_installer(self):
        self.project('ember')
        self.local_runner(status=7)
        result = self.run_script('-B', '-L', '-q')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(self.lines(), ['local:ember build --release'])

    def test_jsh_rebuilds_even_if_the_version_number_has_not_changed(self):
        self.project('jsh')
        self.env['TEST_RECORD_ARGS'] = '1'
        result = self.run_script('-B', '-q')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('args:jsh:--force --bin-dir ' + str(self.installed), self.lines())

    def test_installer_failure_is_reported_and_does_not_stop_other_projects(self):
        self.project('ember', fail=True)
        self.project('frost')
        result = self.run_script('-B', '-q')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertCountEqual(self.lines(), ['installer:ember', 'installer:frost'])
        self.assertIn('退出码 9', result.stdout)


if __name__ == '__main__':
    unittest.main()
