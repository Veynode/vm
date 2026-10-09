#!/usr/bin/env python3
"""Actual filesystem/archive safety tests; synthetic review does not license a product."""
import importlib.util
import hashlib
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('gate', Path(__file__).with_name('package-reviewed-runtime.py'))
gate = importlib.util.module_from_spec(spec);spec.loader.exec_module(gate)

class ReleaseTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        self.base = Path(temp.name); self.root = self.base/'runtime';self.root.mkdir()
        (self.root/'payload').write_bytes(b'fictional runtime test fixture')
        self.evidence = {}
        for kind in gate.REQUIRED:
            p=self.base/(kind+'.txt');p.write_text('SYNTHETIC TEST ONLY '+kind)
            self.evidence[kind]={'path':p.name,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()}
        self.manifest={'schema':1,'status':'reviewed','scope':'synthetic test only','roots':[{'id':'fixture','path':'runtime','tree_sha256':gate.inventory(self.root)['tree_sha256']}],'evidence':self.evidence,'blockers':[]}
        self.output=self.base/'test.tar.gz'

    def test_blocked_never_creates_archive(self):
        self.manifest['status']='blocked'
        self.assertFalse(gate.package(self.manifest,self.base,self.output)['release_allowed'])
        self.assertFalse(self.output.exists())

    def test_missing_or_modified_evidence_blocks(self):
        self.manifest['evidence']['corresponding_source']=None
        self.assertFalse(gate.package(self.manifest,self.base,self.output)['release_allowed'])
        self.manifest['evidence']['corresponding_source']={'path':'corresponding_source.txt','sha256':'0'*64}
        self.assertFalse(gate.package(self.manifest,self.base,self.output)['release_allowed'])
        self.assertFalse(self.output.exists())

    def test_added_or_changed_artifact_requires_new_review(self):
        (self.root/'unreviewed').write_bytes(b'new')
        self.assertFalse(gate.package(self.manifest,self.base,self.output)['release_allowed'])
        (self.root/'unreviewed').unlink();(self.root/'payload').write_bytes(b'altered')
        self.assertFalse(gate.package(self.manifest,self.base,self.output)['release_allowed'])

    def test_external_link_cannot_pass_with_fresh_hash(self):
        (self.root/'escape').symlink_to('../corresponding_source.txt')
        self.manifest['roots'][0]['tree_sha256']=gate.inventory(self.root)['tree_sha256']
        self.assertFalse(gate.package(self.manifest,self.base,self.output)['release_allowed'])
        self.assertFalse(self.output.exists())

    def test_internal_link_preserved_and_all_evidence_packaged(self):
        (self.root/'alias').symlink_to('payload')
        self.manifest['roots'][0]['tree_sha256']=gate.inventory(self.root)['tree_sha256']
        self.assertTrue(gate.package(self.manifest,self.base,self.output)['release_allowed'])
        with tarfile.open(self.output) as archive:
            self.assertEqual(archive.extractfile('runtime/fixture/payload').read(),b'fictional runtime test fixture')
            self.assertTrue(archive.getmember('runtime/fixture/alias').issym())
            self.assertEqual(archive.getmember('runtime/fixture/alias').linkname,'payload')
            self.assertIn('release-evidence/manifest.json',archive.getnames())
            for kind in gate.REQUIRED:self.assertIn('release-evidence/'+kind+'/'+kind+'.txt',archive.getnames())

    def test_mutation_during_tar_never_publishes(self):
        from unittest.mock import patch
        original = tarfile.TarFile.add
        def mutate(archive, name, *args, **kwargs):
            result = original(archive, name, *args, **kwargs)
            if str(name).endswith('product_license.txt'):
                (self.root/'payload').write_bytes(b'changed after archive read')
            return result
        with patch.object(tarfile.TarFile, 'add', mutate):
            with self.assertRaises(ValueError):gate.package(self.manifest,self.base,self.output)
        self.assertFalse(self.output.exists())

    def test_output_never_overwrites_existing(self):
        self.output.write_bytes(b'preserve')
        with self.assertRaises(ValueError):gate.package(self.manifest,self.base,self.output)
        self.assertEqual(self.output.read_bytes(),b'preserve')

    def test_symlinked_review_file_rejected(self):
        p=self.base/'product_license.txt';p.unlink();p.symlink_to('license_notices.txt')
        with self.assertRaises(ValueError):gate.check(self.manifest,self.base)

    def test_cli_manifest_size_and_type_fail_bounded_without_output(self):
        import os, subprocess, sys, json
        huge=self.base/'huge.json'
        with huge.open('wb') as out:out.truncate(1024*1024+1)
        fifo=self.base/'manifest-fifo';os.mkfifo(fifo)
        link=self.base/'manifest-link';link.symlink_to(huge)
        malformed=self.base/'malformed.json';malformed.write_text('{private-sentinel-invalid')
        for path in [huge, fifo, link, malformed, self.root]:
            with self.subTest(path=path.name):
                child=subprocess.run([sys.executable, str(Path(gate.__file__).resolve()), '--manifest', str(path), '--output',str(self.output)],capture_output=True,text=True,timeout=2)
                self.assertEqual(child.returncode,2)
                self.assertFalse(json.loads(child.stdout)['release_allowed'])
                self.assertNotIn('private-sentinel',child.stdout+child.stderr)
                self.assertFalse(self.output.exists())

    def test_special_file_and_parent_traversal_rejected(self):
        import os
        os.mkfifo(self.root/'pipe')
        with self.assertRaises(ValueError):gate.check(self.manifest,self.base)
        (self.root/'pipe').unlink();self.manifest['roots'][0]['path']='../runtime'
        with self.assertRaises(ValueError):gate.check(self.manifest,self.base)

if __name__ == '__main__':unittest.main()
