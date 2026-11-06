#!/usr/bin/env python3
"""Exercise the physical-flash regression the app-only size gate missed."""
import importlib.util
import sys
sys.dont_write_bytecode = True
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('layout', Path(__file__).with_name('check-flash-layout.py'))
layout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(layout)


class LayoutTests(unittest.TestCase):
    def check_table(self, table, size=0x300000):
        with tempfile.TemporaryDirectory() as directory:
            image = Path(directory) / 'app.bin'
            with image.open('wb') as stream:
                stream.truncate(size)
            csv = Path(directory) / 'partitions.csv'
            csv.write_text(table)
            layout.check(image, csv, 4 * 1024**2)

    def test_shipped_c3_layout_fits_physical_flash(self):
        table = Path(__file__).resolve().parent.parent / 'partitions.esp32c3.csv'
        self.check_table(table.read_text(), size=1)

    def test_store_beyond_flash_even_when_app_fits(self):
        with self.assertRaisesRegex(ValueError, 'exceeds .* flash'):
            self.check_table('factory,app,factory,0x10000,3M\nstore,data,nvs,,1M\n')

    def test_image_beyond_factory(self):
        with self.assertRaisesRegex(ValueError, 'app exceeds'):
            self.check_table('factory,app,factory,0x10000,3M\n', 0x300001)

    def test_overlap(self):
        with self.assertRaisesRegex(ValueError, 'overlaps'):
            self.check_table('factory,app,factory,0x10000,3M\nstore,data,nvs,0x300000,64K\n')

    def test_exact_flash_end_and_auto_offsets(self):
        self.check_table('nvs,data,nvs,0x9000,24K\nphy,data,phy,,4K\n'
                         'factory,app,factory,,3M\nstore,data,nvs,,960K\n')


if __name__ == '__main__':
    unittest.main()
