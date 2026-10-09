#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import os,subprocess,sys,tempfile,time,unittest
R=Path(__file__).resolve().parents[1]
def load(n,p):
 s=importlib.util.spec_from_file_location(n,p);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
B=load('mesa_build_test',R/'scripts/linux-vulkan-mesa25-build.py')
P=load('mesa_prepare_test',R/'scripts/linux-vulkan-mesa25-prepare.py')
class Tests(unittest.TestCase):
 def test_exact_build_child_success_and_eof(self):
  with tempfile.TemporaryDirectory() as d:
   r=B.supervise([sys.executable,'-c','print("fixture")'],Path(d)/'log',2,100)
   self.assertTrue(r['passed']);self.assertTrue(r['owned_child_reaped']);self.assertEqual(r['exit_code'],0)
 def test_output_overflow_is_bounded_and_reaped(self):
  with tempfile.TemporaryDirectory() as d:
   p=Path(d)/'log';r=B.supervise([sys.executable,'-c','import sys,time;sys.stdout.write("x"*8192);sys.stdout.flush();time.sleep(5)'],p,2,100)
   self.assertFalse(r['passed']);self.assertEqual(r['failure'],'output_limit');self.assertEqual(p.stat().st_size,100);self.assertTrue(r['owned_child_reaped'])
 def test_deadline_is_failure_and_reaped(self):
  with tempfile.TemporaryDirectory() as d:
   r=B.supervise([sys.executable,'-c','import time;time.sleep(5)'],Path(d)/'log',.1,100)
   self.assertFalse(r['passed']);self.assertEqual(r['failure'],'deadline');self.assertTrue(r['owned_child_reaped'])
 def test_exited_leader_with_descendant_stdout_is_killed_on_deadline(self):
  with tempfile.TemporaryDirectory() as d:
   pidfile=Path(d)/'child.pid'
   code="import subprocess,sys,os; p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)']);open(sys.argv[1],'w').write(str(p.pid));os._exit(0)"
   r=B.supervise([sys.executable,'-c',code,str(pidfile)],Path(d)/'log',.3,100)
   self.assertEqual(r['failure'],'deadline');self.assertFalse(r['passed']);self.assertTrue(r['owned_child_reaped']);self.assertTrue(r['forced_group_kill'])
   pid=int(pidfile.read_text());end=time.monotonic()+3
   while time.monotonic()<end:
    try:os.kill(pid,0)
    except ProcessLookupError:break
    time.sleep(.02)
   else:self.fail('descendant survived owned group cleanup')
 def test_pid1_guard_prevents_host_mount_or_build(self):
  r=subprocess.run(['/bin/sh'],input=P.init_script(),text=True,capture_output=True,timeout=2)
  self.assertEqual(r.returncode,90);self.assertEqual(r.stdout+r.stderr,'')
if __name__=='__main__':unittest.main()
