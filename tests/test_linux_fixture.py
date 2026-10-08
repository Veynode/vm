"""Artifact format checks protect the EFI-zboot regression observed on a real VZ boot."""
import gzip,importlib.util,pathlib,struct,unittest
ROOT=pathlib.Path(__file__).parents[1]
spec=importlib.util.spec_from_file_location('fixture',ROOT/'scripts/build-linux-fixture.py');fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
class FixtureTests(unittest.TestCase):
 def test_cpio_preserves_unicode_length_and_padding(self):
  data=fixture.entry('test/你好',b'abc',0o100755)
  self.assertEqual(data[:6],b'070701');fields=[int(data[6+i*8:14+i*8],16) for i in range(13)]
  self.assertEqual(fields[1],0o100755);self.assertEqual(fields[6],3)
  start=(110+fields[11]+3)&~3;self.assertEqual(data[start:start+3],b'abc');self.assertEqual(len(data)%4,0)
 def test_prepared_kernel_is_raw_arm64_if_available(self):
  path=ROOT/'.local/media/alpine-3.24.2/Image'
  if not path.exists():self.skipTest('downloaded media absent; run prepare-linux.py')
  with path.open('rb') as f:header=f.read(64)
  self.assertEqual(header[56:60],b'ARM\x64');self.assertNotEqual(header[4:8],b'zimg')
if __name__=='__main__':unittest.main()
