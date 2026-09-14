import pathlib
import tempfile
import unittest
from models import ModelManager

class Models(unittest.TestCase):
    def test_catalog_only_gguf_deduplicates_and_confines_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root=pathlib.Path(directory); folder=root/'models';folder.mkdir()
            (folder/'chat.gguf').write_bytes(b'gguf')
            (folder/'chat-alias.gguf').symlink_to(folder/'chat.gguf')
            (folder/'notes.txt').write_text('ignored')
            (root/'outside.gguf').write_bytes(b'x')
            (folder/'escape.gguf').symlink_to(root/'outside.gguf')
            m=ModelManager(folder,'missing',18088,root)
            self.assertEqual(len(m.catalog()),1)
            self.assertEqual(m.catalog()[0]['bytes'],4)
            with self.assertRaises(ValueError):m.switch('../outside.gguf')
    def test_loading_rejects_second_switch(self):
        with tempfile.TemporaryDirectory() as directory:
            root=pathlib.Path(directory);(root/'chat.gguf').touch()
            m=ModelManager(root,'missing',18088,root)
            m.state['status']='loading'
            with self.assertRaises(ValueError):m.switch('chat.gguf')

if __name__=='__main__':unittest.main()
