"""Assert version selection and that transition failures cannot become passes."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import baseline
import transitions


def release(tag, **kwargs):
    return dict(tag_name=tag, draft=False, prerelease=False) | kwargs


class SelectionTests(unittest.TestCase):
    def test_semantic_order_samples_only_candidate_and_previous_lines(self):
        tags = [release(tag) for tag in ['v0.6.18', 'v0.7.0', 'v0.7.1', 'v0.7.10', 'v0.7.2']]
        tags += [release('v0.8.0'), release('v0.7.11', draft=True), release('v0.7.12', prerelease=True), release('v1.0.0-rc.1')]
        self.assertEqual([r['tag_name'] for r in baseline.select_releases(tags, '0.7.3')], ['v0.7.10', 'v0.7.2', 'v0.6.18'])

    def test_previous_line_tracks_new_patches_without_a_permanent_pin(self):
        tags = [release('v0.6.18'), release('v0.6.19'), release('v0.7.0')]
        self.assertEqual([r['tag_name'] for r in baseline.select_releases(tags, '0.7.1')], ['v0.7.0', 'v0.6.19'])

    def test_minor_bump_without_a_published_current_line(self):
        tags = [release('v0.6.18'), release('v0.7.0'), release('v0.7.1')]
        self.assertEqual([r['tag_name'] for r in baseline.select_releases(tags, '0.8.0-rc.1')], ['v0.7.1'])

    def test_window_moves_after_a_minor_release(self):
        tags = [release(tag) for tag in ['v0.6.18', 'v0.7.1', 'v0.8.0', 'v0.8.1', 'v0.9.0']]
        self.assertEqual([r['tag_name'] for r in baseline.select_releases(tags, '0.8.2')], ['v0.8.1', 'v0.8.0', 'v0.7.1'])

    def test_unsupported_candidate_version_is_not_silently_reinterpreted(self):
        for candidate in ['1.0.0', '0.0.1', 'not-a-version']:
            with self.subTest(candidate=candidate), self.assertRaises(ValueError):
                baseline.select_releases([], candidate)

    def test_missing_required_history_is_an_error(self):
        with self.assertRaises(ValueError):
            baseline.select_releases([release('v0.6.18'), release('v0.8.0')], '0.8.1')

    def test_pagination_pins_once_and_provisions_each_selected_version(self):
        pages = [[release('v0.7.1')] * 100, [release('v0.7.0'), release('v0.6.18')]]
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(baseline, 'read_json', side_effect=pages) as query, \
                mock.patch.object(baseline, 'provision') as provision, \
                mock.patch.dict(baseline.os.environ, {'GITHUB_OUTPUT': str(Path(directory)/'outputs')}):
            output = Path(directory)/'baseline'
            manifest = Path(directory)/'Cargo.toml'
            manifest.write_text('[workspace.package]\nversion = "0.7.2"\n')
            baseline.resolve_matrix(output, manifest)
            self.assertEqual(query.call_count, 2)
            self.assertTrue(query.call_args.args[0].endswith('page=2'))
            self.assertEqual(provision.call_count, 3)
            self.assertEqual(json.loads((output/'selection.json').read_text()), ['v0.7.1', 'v0.7.0', 'v0.6.18'])
            self.assertIn('versions=', (Path(directory)/'outputs').read_text())
            self.assertEqual(json.loads((output/'policy.json').read_text())['candidate'], '0.7.2')

    def test_bump_manifest_changes_are_not_filtered_out(self):
        root = Path(__file__).resolve().parents[2]
        workflow = (root/'.github/workflows/check.yml').read_text()
        self.assertIn("- '!docs/**'", workflow)
        self.assertIn("if: needs.changes.outputs.code == 'true'\n    uses: ./.github/workflows/sdk-runtime-compat.yml", workflow)

    def test_guest_markers_do_not_use_the_reserved_prefix(self):
        root = Path(__file__).resolve().parent
        for path in (root/'scenarios').rglob('*'):
            if path.is_file() and path.suffix in {'.py', '.rs', '.go', '.cjs', '.rb'}:
                text = path.read_text()
                for marker in ['MSB_COMPAT_MARKER', 'MSB_COMPAT_SENTINEL', 'MSB_COMPAT_EMPTY']:
                    self.assertNotIn(marker, text, str(path))


class TransitionTests(unittest.TestCase):
    def exercise(self, fault=None, cleanup_fault=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root/'output'
            output.mkdir()
            runtimes = {}
            for generation in ['released', 'candidate']:
                runtime = root/generation
                runtime.mkdir()
                (runtime/'msb').write_text(generation)
                (runtime/'libkrunfw.so.5.6.1').write_text('firmware')
                runtimes[generation] = runtime/'msb', runtime/'libkrunfw.so.5.6.1'
            states = {}
            events = []
            def allocate(**kwargs):
                path = root/f'fixture-{len(states)}'
                path.mkdir()
                states[str(path/'home')] = dict(schema='old', active=False)
                return str(path)
            def env(home, *args):
                return {'MSB_HOME': str(home)}
            def schema(home):
                return states[str(home)]['schema']
            def run(command, environment, cwd, log, *args):
                command = list(map(str, command))
                home = environment['MSB_HOME']
                state = states[home]
                events.append(command)
                if command[0] == 'sdk-lifecycle':
                    if fault == 'sdk-refusal':
                        raise RuntimeError('old SDK refuses newer catalog')
                    if fault == 'sdk-mutation':
                        state['schema'] = 'sdk-changed'
                    Path(environment['MSB_COMPAT_REPORT']).write_text(json.dumps({'status':'passed', 'passed':['restart']}))
                    Path(environment['MSB_COMPAT_RUNTIME_REPORT']).write_text(json.dumps({'sandbox':'retained', 'runtimes':[{'pid':2, 'sha256':environment['MSB_COMPAT_RUNTIME_SHA256']}]}))
                elif len(command) > 1 and command[1].endswith('verify_runtime.py'):
                    name = command[-1]
                    pid = 3 if fault == 'replaced-old-vm' and 'MSB_COMPAT_RUNTIME_IDENTITIES' in environment else 1
                    with open(environment['MSB_COMPAT_RUNTIME_REPORT'], 'a') as stream:
                        stream.write(json.dumps({'sandbox':name, 'runtimes':[{'pid':pid,'sha256':environment['MSB_COMPAT_RUNTIME_SHA256']}]})+'\n')
                elif command[1:3] == ['create', 'image'] and 'active' in command:
                    state['active'] = True
                elif command[1:3] == ['stop', 'active']:
                    state['active'] = False
                elif command[1:3] == ['start', 'retained']:
                    if state['active']:
                        if fault == 'stop-all-refusal':
                            raise RuntimeError('catalog upgrade requires stopped sandboxes')
                        if fault == 'live-migration':
                            state['schema'] = 'new'
                    else:
                        state['schema'] = 'new'
                elif command[1:3] == ['exec', 'retained'] and 'test "$(cat /root/compat-marker)' in command[-1] and fault == 'lost-data':
                    raise RuntimeError('retained marker missing')
            def cleanup(*args):
                if cleanup_fault:
                    raise RuntimeError('cleanup failure')
            def validate(report, evidence, digest):
                self.assertEqual(report['status'], 'passed')
                self.assertTrue(evidence)
                self.assertEqual(json.loads(evidence[0])['runtimes'][0]['sha256'], digest)
            with mock.patch.object(transitions.tempfile, 'mkdtemp', side_effect=allocate), contextlib.redirect_stdout(io.StringIO()):
                cases = transitions.execute(commands={sdk:(['sdk'],{},root) for sdk in ['candidate','released']},
                    manifest={'language':'rust','sdks':{'candidate':{'version':'0.7.1'},'released':{'version':'0.7.0'}}},
                    payload=root, runtimes=runtimes, scripts=root, output=output, image='image',
                    environment=env, run=run, schema=schema, cleanup=cleanup,
                    lifecycle_command=lambda *args:['sdk-lifecycle'], validate_report=validate)
            return cases, events

    def test_all_transitions_require_real_success_and_both_sdk_directions(self):
        cases, _ = self.exercise()
        self.assertEqual(len(cases), 3)
        self.assertTrue(all(case['status'] == 'passed' for case in cases))

    def test_stop_all_refusal_is_failure_but_recovery_is_still_exercised(self):
        cases, events = self.exercise('stop-all-refusal')
        self.assertEqual(cases[0]['status'], 'failed')
        self.assertIn('requires stopped', cases[0]['error'])
        self.assertTrue(any(cmd[1:3] == ['stop','active'] for cmd in events))
        self.assertTrue(any(step['step']=='new-cli-start-retained' and step['status']=='passed' for step in cases[0]['steps']))

    def test_old_vm_replacement_or_live_schema_mutation_fails(self):
        for fault in ['replaced-old-vm', 'live-migration']:
            with self.subTest(fault=fault):
                cases, _ = self.exercise(fault)
                self.assertEqual(cases[0]['status'], 'failed')

    def test_post_upgrade_sdk_refusal_and_mutation_are_failures(self):
        for fault in ['sdk-refusal', 'sdk-mutation']:
            with self.subTest(fault=fault):
                cases, _ = self.exercise(fault)
                self.assertTrue(all(case['status']=='failed' for case in cases[1:]))

    def test_data_loss_fails_all_transitions(self):
        cases, _ = self.exercise('lost-data')
        self.assertTrue(all(case['status']=='failed' for case in cases))

    def test_cleanup_failure_cannot_mask_primary_failure(self):
        cases, _ = self.exercise('stop-all-refusal', cleanup_fault=True)
        self.assertIn('requires stopped', cases[0]['error'])
        self.assertIn('cleanup failure', cases[0]['cleanup_error'])
        self.assertTrue(all(case['status']=='failed' for case in cases))

    def test_binary_install_does_not_truncate_a_live_mapping(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, fw = root/'source', root/'libkrunfw.so.5.6.1'
            source.write_bytes(b'old')
            fw.write_bytes(b'firmware')
            transitions.install(root/'home', source, fw)
            with (root/'home/bin/msb').open('rb') as retained:
                source.write_bytes(b'new')
                transitions.install(root/'home', source, fw)
                self.assertEqual(retained.read(), b'old')
                self.assertEqual((root/'home/bin/msb').read_bytes(), b'new')


if __name__ == '__main__':
    unittest.main()
