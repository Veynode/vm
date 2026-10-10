#!/usr/bin/env python3
"""Offline parser/host-guard checks, not actual WSI acceptance."""
import importlib.util
from pathlib import Path
import json
import signal
import subprocess
import sys
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
 def test_signal_exit_requires_matching_force_receipt(self):
  p=subprocess.Popen([sys.executable,'-c','import time; print("ready",flush=True); time.sleep(5)'],stdout=subprocess.PIPE,text=True)
  try:
   self.assertEqual(p.stdout.readline(),'ready\n');p.kill();code=p.wait(timeout=2)
   self.assertEqual(code,-signal.SIGKILL)
   # Rust ExitStatus.code() maps this actual signal exit to null, not exit0.
   status={'config':{'id':'owned'},'state':'stopped','exit_code':None,'runtime':{'reason':'force_stopped'}}
   receipt={'forced':True,'state':'stopped'}
   self.assertEqual(T.retirement_basis(status,'owned',receipt),'synchronous-force-receipt-and-terminal-view')
   for s,v,r in [(status,'owned',None),(status,'other',receipt),({**status,'pid':p.pid},'owned',receipt),({**status,'runtime':{}},'owned',receipt),(status,'owned',{'forced':False,'state':'stopped'})]:
    with self.assertRaises(AssertionError):T.retirement_basis(s,v,r)
   self.assertEqual(T.retirement_basis({**status,'exit_code':0},'owned'),'terminal-exit-code')
  finally:
   if p.poll() is None:p.kill();p.wait(timeout=2)
   p.stdout.close()
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
 def test_virtual_virgl_name_requires_pixels_not_apple_gl_string(self):
  sample=S.replace(b'Device: zink Vulkan Venus (Apple M1 Pro)',b'Device: virgl')
  rows=''.join('XRENDER_CASE '+json.dumps({'name':str(i),'passed':True,'mismatched_pixels':0})+'\n'for i in range(15))
  sample+=rows.encode()+b'XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}\n'
  self.assertTrue(T.workload(sample,'virgl')['application_completed'])
  with self.assertRaises(AssertionError):T.workload(sample.replace(b'Device: virgl',b'Device: llvmpipe'),'virgl')
  with self.assertRaises(AssertionError):T.workload(sample.replace(b'"name": "1"',b'"name": "0"'),'virgl')
 def test_actual_debug_prefix_and_emitter_boundary(self):
  debug=S.replace(b'[shading]',b'Info: [shading]')
  self.assertTrue(T.workload(debug)['application_completed'])
  with self.assertRaises(AssertionError):T.workload(debug.replace(b'Info: [shading]',b'unrelated Info: [shading]'))
  # Execute the emitter after the upstream app's deliberately unterminated progress line.
  line=next(x for x in P.WSI.splitlines()if 'VEYNODE_WSI_WINDOW_READY' in x)
  r=subprocess.run(['/bin/sh','-c',"printf 'Info: [shading] duration=60.0:'; "+line],capture_output=True,check=True)
  self.assertIn(b'\nVEYNODE_WSI_WINDOW_READY\n',r.stdout)
 def test_guest_pid1_guard_prevents_host_actions(self):
  script=P.guest_script();subprocess.run(['/bin/sh','-n'],input=script,text=True,check=True)
  p=subprocess.run(['/bin/sh'],input=script,text=True,capture_output=True,timeout=2)
  self.assertEqual(p.returncode,90);self.assertEqual(p.stdout,'');self.assertEqual(p.stderr,'')
if __name__=='__main__':unittest.main()
