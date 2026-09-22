"""JSONL records must be split only at the line delimiter."""
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

import run


class JsonlRecordsTest(unittest.TestCase):
    def test_unicode_next_line_inside_json_string_is_not_record_break(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / 'trace.jsonl'
            rows = [{'text': 'before\u0085after'}, {'status': 200}]
            path.write_text(''.join(json.dumps(row, ensure_ascii=False) + '\n' for row in rows))
            self.assertEqual(run.read_jsonl(path), rows)


if __name__ == '__main__':
    unittest.main()
