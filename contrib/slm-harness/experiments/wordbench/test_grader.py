import shutil
import tempfile
import unittest
from pathlib import Path

from docx import Document

from grader import grade
from tasks import build_task_specs


class GraderTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(__file__).resolve().parents[2] / 'artifacts/wordbench/tasks'

    def test_gold_copy_passes(self):
        spec = build_task_specs()[0]
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / 'output.docx'
            shutil.copyfile(self.root / spec['id'] / 'gold.docx', out)
            result = grade(spec, self.root / spec['id'] / 'input.docx', self.root / spec['id'] / 'gold.docx', out)
            self.assertTrue(result['artifact_valid'])
            self.assertTrue(result['structure_passed'])
            self.assertTrue(result['content_passed'])

    def test_protected_text_mutation_fails(self):
        spec = build_task_specs()[4]
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / 'output.docx'
            shutil.copyfile(self.root / spec['id'] / 'gold.docx', out)
            doc = Document(out)
            doc.paragraphs[1].text = '保密级别：公开'
            doc.save(out)
            result = grade(spec, self.root / spec['id'] / 'input.docx', self.root / spec['id'] / 'gold.docx', out)
            self.assertFalse(result['content_passed'])
            self.assertIn('protected_text_missing', result['failures'])

    def test_missing_table_fails_structure(self):
        spec = build_task_specs()[2]
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / 'output.docx'
            shutil.copyfile(self.root / spec['id'] / 'input.docx', out)
            result = grade(spec, self.root / spec['id'] / 'input.docx', self.root / spec['id'] / 'gold.docx', out)
            self.assertFalse(result['structure_passed'])
            self.assertIn('table_rows_mismatch', result['failures'])


if __name__ == '__main__':
    unittest.main()
