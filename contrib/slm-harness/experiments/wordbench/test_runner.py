import unittest

from run_pilot import classify_render


class RunnerTests(unittest.TestCase):
    def test_render_failure_is_evidence_gap(self):
        result = classify_render({'ok': False, 'returncode': 1, 'pages': []})
        self.assertEqual(result, 'render_evidence_gap')

    def test_render_success_is_pass(self):
        result = classify_render({'ok': True, 'returncode': 0, 'pages': ['page-1.png']})
        self.assertEqual(result, 'rendered')


if __name__ == '__main__':
    unittest.main()
