import unittest

from tasks import CATEGORIES, build_task_specs


class TaskManifestTests(unittest.TestCase):
    def test_manifest_has_24_unique_tasks_and_six_categories(self):
        specs = build_task_specs()
        self.assertEqual(len(specs), 24)
        self.assertEqual(len({s['id'] for s in specs}), 24)
        self.assertEqual({s['category'] for s in specs}, set(CATEGORIES))
        for category in CATEGORIES:
            self.assertEqual(sum(s['category'] == category for s in specs), 4)

    def test_first_six_are_pilot_tasks(self):
        specs = build_task_specs()
        self.assertEqual([s['id'] for s in specs[:6]], [
            'content-extract', 'structure-headings', 'table-create',
            'style-unify', 'fidelity-protected', 'delivery-brief',
        ])
        self.assertEqual(len({s['category'] for s in specs[:6]}), 6)


if __name__ == '__main__':
    unittest.main()
