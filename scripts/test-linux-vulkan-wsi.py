#!/usr/bin/env python3
"""Offline parser/host-guard checks, not actual WSI acceptance."""
import importlib.util
from pathlib import Path
import subprocess
import unittest
ROOT=Path(__file__).resolve().parents[1]
def load(n,p):
 s=importlib.util.spec_from_file_location(n,p);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
P=load('wsi_prepare_test',ROOT/'scripts/linux-vulkan-wsi-prepare.py')
T=load('wsi_parser_test',ROOT/'scripts/linux-vulkan-wsi-test.py')
S=b'''WSI_GL_BEGIN
Device: zink Vulkan Venus (Apple M1 Pro)
Accelerated: yes
WSI_GL_END
WSI_APP_LOG_BEGIN
Device Name: Virtio-GPU Venus (Apple M1 Pro)
Vendor ID: 0x106B
XcbNativeSystem: Using root visual
SwapchainWindowSystem: Swapchain contains 3 images
[shading] duration=60.0:shading=phong: FPS: 20 FrameTime: 50.000 ms
WSI_APP_LOG_END
WSI_CASE_EXIT shading-phong 0
VEYNODE_WSI_READY
'''
class Tests(unittest.TestCase):
 def test_parser_accepts_completed_scene_not_visual_conformance(self):
  r=T.workload(S);self.assertTrue(r['application_completed']);self.assertFalse(r['visual_conformance_validated'])
 def test_no_completion_no_pass(self):self.assertIsNone(T.workload(S.replace(b'VEYNODE_WSI_READY',b'NOT_READY')))
 def test_headless_cannot_pass(self):
  with self.assertRaises(AssertionError):T.workload(S.replace(b'XcbNativeSystem:',b'HeadlessSystem:'))
 def test_software_desktop_or_device_rejected(self):
  for old,new in [(b'Device: zink',b'Device: llvmpipe zink'),(b'Virtio-GPU Venus (Apple M1 Pro)',b'llvmpipe')]:
   with self.subTest(old=old),self.assertRaises(AssertionError):T.workload(S.replace(old,new))
 def test_failed_or_duplicate_scene_rejected(self):
  for sample in [S.replace(b'phong 0',b'phong 1'),S+ b'WSI_CASE_EXIT shading-phong 0\n',S.replace(b'FPS: 20',b'FPS: 0')]:
   with self.assertRaises(AssertionError):T.workload(sample)
 def test_guest_pid1_guard_prevents_host_actions(self):
  script=P.guest_script();subprocess.run(['/bin/sh','-n'],input=script,text=True,check=True)
  p=subprocess.run(['/bin/sh'],input=script,text=True,capture_output=True,timeout=2)
  self.assertEqual(p.returncode,90);self.assertEqual(p.stdout,'');self.assertEqual(p.stderr,'')
if __name__=='__main__':unittest.main()
