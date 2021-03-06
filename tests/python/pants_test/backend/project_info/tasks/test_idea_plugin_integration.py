# Copyright 2014 Pants project contributors (see CONTRIBUTORS.md).
# Licensed under the Apache License, Version 2.0 (see LICENSE).

import json
import os
from xml.dom import minidom

from pants.backend.project_info.tasks.idea_plugin_gen import IDEA_PLUGIN_VERSION
from pants.base.build_environment import get_buildroot
from pants.base.cmd_line_spec_parser import CmdLineSpecParser
from pants.base.specs import DescendantAddresses, SiblingAddresses, SingleAddress
from pants.testutil.pants_run_integration_test import PantsRunIntegrationTest
from pants.util.contextutil import temporary_file


class IdeaPluginIntegrationTest(PantsRunIntegrationTest):
  def _do_check(self, project_dir_path, expected_project_path, expected_targets,
                incremental_import=None):
    """Check to see that the project contains the expected source folders."""

    workspace_file = os.path.join(
      project_dir_path, ".idea", 'workspace.xml'
    )
    self.assertTrue(os.path.exists(workspace_file))
    dom = minidom.parse(workspace_file)
    self.assertEqual(1, len(dom.getElementsByTagName("project")))
    project = dom.getElementsByTagName("project")[0]

    self.assertEqual(1, len(project.getElementsByTagName('component')))
    component = project.getElementsByTagName('component')[0]

    actual_properties = component.getElementsByTagName('property')
    valid_property_names = {
        'targets',
        'project_path',
        'pants_idea_plugin_version',
        'incremental_import',
        'dep_as_jar'
      }
    self.assertTrue({p.getAttribute('name')
                     for p in actual_properties}.issubset(valid_property_names))
    self.assertTrue(len(actual_properties) >= 3)

    self.assertEqual('targets', actual_properties[0].getAttribute('name'))
    actual_targets = json.loads(actual_properties[0].getAttribute('value'))
    abs_expected_address_specs = [os.path.join(get_buildroot(), relative_spec) for relative_spec in expected_targets]
    self.assertEqual(abs_expected_address_specs, actual_targets)

    self.assertEqual('project_path', actual_properties[1].getAttribute('name'))
    actual_project_path = actual_properties[1].getAttribute('value')
    self.assertEqual(os.path.join(get_buildroot(), expected_project_path), actual_project_path)

    self.assertEqual('pants_idea_plugin_version', actual_properties[2].getAttribute('name'))
    self.assertEqual(IDEA_PLUGIN_VERSION, actual_properties[2].getAttribute('value'))

    incremental_import_props = [p
                                for p in actual_properties
                                if p.getAttribute('name') == 'incremental_import']
    if incremental_import is None:
      self.assertEqual(incremental_import_props, [])
    else:
      self.assertEqual([str(incremental_import)], [p.getAttribute('value')
                                                    for p in incremental_import_props])

  def _get_project_dir(self, output_file):
    with open(output_file, 'r') as result:
      return result.readlines()[0].strip()

  def _run_and_check(self, address_specs, incremental_import=None):
    """
    Invoke idea-plugin goal and check for target specs and project in the
    generated project and workspace file.

    :param address_specs: list of address specs
    :return: n/a
    """
    self.assertTrue(address_specs, "targets are empty")
    spec_parser = CmdLineSpecParser(get_buildroot())
    # project_path is always the directory of the first target,
    # which is where intellij is going to zoom in under project view.
    spec = spec_parser.parse_spec(address_specs[0])
    assert isinstance(spec, (SingleAddress, SiblingAddresses, DescendantAddresses))
    project_path = spec.directory

    with self.temporary_workdir() as workdir:
      with temporary_file(root_dir=workdir, cleanup=True) as output_file:
        args = [
            'idea-plugin',
            f'--output-file={output_file.name}',
            '--no-open',
          ]
        if incremental_import is not None:
          args.append(f'--incremental-import={incremental_import}')
        pants_run = self.run_pants_with_workdir(args + address_specs, workdir)
        self.assert_success(pants_run)

        project_dir = self._get_project_dir(output_file.name)
        self.assertTrue(os.path.exists(project_dir), f"{project_dir} does not exist")
        self._do_check(project_dir, project_path, address_specs, incremental_import=incremental_import)

  def test_idea_plugin_single_target(self):
    target = 'examples/src/scala/org/pantsbuild/example/hello:hello'
    self._run_and_check([target])

  def test_idea_plugin_single_directory(self):
    target = 'testprojects/src/python/antlr::'
    self._run_and_check([target])

  def test_idea_plugin_incremental_import(self):
    target = 'testprojects/src/python/antlr::'
    self._run_and_check([target], incremental_import=1337)

  def test_idea_plugin_multiple_targets(self):
    target_a = 'examples/src/scala/org/pantsbuild/example/hello:'
    target_b = 'testprojects/src/python/antlr::'
    self._run_and_check([target_a, target_b])
