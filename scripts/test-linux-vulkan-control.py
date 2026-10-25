#!/usr/bin/env python3
"""Offline admission/oracle regressions, not actual controller/GPU acceptance."""
import hashlib
import importlib.util
import json
import os
import subprocess
from pathlib import Path
import tempfile
import unittest
ROOT=Path(__file__).resolve().parents[1]
def load(name,path):
 s=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
P=load('prep',ROOT/'scripts/linux-vulkan-prepare-control.py')
T=load('test',ROOT/'scripts/linux-vulkan-test-control.py')
class Control(unittest.TestCase):
 def test_pin_valid_and_mutated(self):
  with tempfile.TemporaryDirectory() as d:
   p=Path(d).resolve()/'regular';p.write_bytes(b'actualfixture');p.chmod(0o600);sha=hashlib.sha256(p.read_bytes()).hexdigest()
   self.assertEqual(P.pinned(p,sha)['sha256'],sha);p.write_bytes(b'changed')
   with self.assertRaisesRegex(ValueError,'pin mismatch'):P.pinned(p,sha)
 def test_fifo_symlink_hardlink_and_growth_bound_rejected(self):
  with tempfile.TemporaryDirectory() as d:
   p=Path(d).resolve();fifo=p/'fifo';os.mkfifo(fifo)
   with self.assertRaisesRegex(ValueError,'unsafe'):P.pinned(fifo,'0'*64)
   f=p/'f';f.write_bytes(b'x'*32);link=p/'l';link.symlink_to(f)
   with self.assertRaisesRegex(ValueError,'noncanonical'):P.pinned(link,'0'*64)
   with self.assertRaisesRegex(ValueError,'unsafe'):P.pinned(f,'0'*64,16)
   os.link(f,p/'hard')
   with self.assertRaisesRegex(ValueError,'unsafe'):P.pinned(f,'0'*64)
 def test_guest_init_refuses_non_pid1_before_mount(self):
  r=subprocess.run(['/bin/sh'],input=P.guest_script(),text=True,capture_output=True,timeout=2)
  self.assertEqual(r.returncode,90);self.assertEqual(r.stdout,'');self.assertEqual(r.stderr,'')
 def test_actual_archived_output_parser_and_failures(self):
  triangle=(ROOT/'archive/docs-evidence/linux-vulkan-investigation/pixel-selected.log').read_text()
  vkmark=(ROOT/'archive/docs-evidence/linux-vulkan-application/v2/selected.log').read_text()
  portion=vkmark[vkmark.index('VKMARK_FEATURES_BEGIN'):vkmark.index('VKMARK_APPLICATION_DONE')+len('VKMARK_APPLICATION_DONE')]
  data=(triangle+'\n'+portion+'\nVEYNODE_CONTROL_VENUS_READY\n').encode()
  self.assertTrue(T.workload(data)['vkmark']['workload_completed'])
  for altered in [data.replace(b'mismatches=0',b'mismatches=1',1),data.replace(b'Device Name:    Virtio-GPU Venus (Apple M1 Pro)',b'Device Name:    llvmpipe'),data+b'VEYNODE_CONTROL_VENUS_READY\n']:
   self.assertNotEqual(altered,data)
   with self.assertRaises(AssertionError):T.workload(altered)
  with self.assertRaises(AssertionError):T.workload(data.replace(b'Virtio-GPU Venus (Apple M1 Pro)',b'lavapipe'))
  self.assertIsNone(T.workload(data.replace(b'VEYNODE_CONTROL_VENUS_READY\n',b'')))
 def test_runtime_requires_independent_host_precont_proofs(self):
  p={'configured':'venus-moltenvk-experimental','host_supported':True,'guest_verified':False,'runtime_revision':'owned-group-v1','qmp_gpu_properties_verified':True,'ipa_granule_requested_bytes':4096,'hostmem_aperture_bytes':268435456,'renderer_ownership':'inherits-worker-process-group','host_admission':{'architecture':'aarch64','public_hv_config_4k_readback':True,'vm_created_by_probe':False}}
  self.assertEqual(T.graphics_proof({'runtime':{'graphics_profile':p}}),p)
  for field,value in [('guest_verified',True),('qmp_gpu_properties_verified',False),('ipa_granule_requested_bytes',16384),('runtime_revision','unknown')]:
   with self.assertRaises(AssertionError):T.graphics_proof({'runtime':{'graphics_profile':{**p,field:value}}})
 def test_observer_execution_and_reconnect_are_rejected(self):
  info={'transport':'virtio-serial-private-observer-v1','transport_reconnect':False,'os':'linux','arch':'arm64','capabilities':dict.fromkeys(['exec','async_exec','files','shutdown','desktop','descendant_containment'],False)}
  T.guest_observer(info)
  with self.assertRaises(AssertionError):T.guest_observer({**info,'transport_reconnect':True})
  info['capabilities']['exec']=True
  with self.assertRaises(AssertionError):T.guest_observer(info)
if __name__=='__main__':unittest.main()
