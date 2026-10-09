#!/usr/bin/env python3
"""Real local Unix/PID/NOTE_EXIT components; never load a renderer or GPU."""
import importlib.util
import os
from pathlib import Path
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest
S=importlib.util.spec_from_file_location('life',Path(__file__).with_name('linux-vulkan-test-host-runtime.py'))
L=importlib.util.module_from_spec(S);S.loader.exec_module(L)
class Lifecycle(unittest.TestCase):
 @classmethod
 def setUpClass(cls):
  cls.temp=tempfile.TemporaryDirectory(prefix='vn-ack-test-');cls.path=Path(cls.temp.name)
  cls.terminal,cls.parent=L.waiter(cls.path)
  cls.wrapper=L.compile_observer(cls.path,'/bin/echo',300)
 @classmethod
 def tearDownClass(cls):cls.temp.cleanup()
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory(prefix='vn-ack-');self.sock=Path(self.tmp.name)/'o'
  self.listener=L.listener(self.sock);self.kq=select.kqueue();self.registered=set();self.events=[]
  # Test runner is the exact trusted parent. Production instead begins at QEMU.
  self.known={os.getpid():{'pid':os.getpid()}};self.children=[]
 def tearDown(self):
  for p in self.children:
   if p.poll() is None:p.kill();p.wait(timeout=2)
   if p.stdout:p.stdout.close()
  self.listener.close();self.kq.close();self.tmp.cleanup()
 def spawn(self,**kw):
  env={**os.environ,'VN_RENDER_OBSERVER_SOCKET':str(self.sock),'VN_RENDER_OBSERVER_PID':str(os.getpid())}
  env.update(kw.pop('env',{}))
  p=subprocess.Popen([str(self.wrapper),'EXECUTED'],env=env,stdout=subprocess.PIPE,**kw);self.children.append(p);return p
 def accept(self):
  self.assertTrue(select.select([self.listener],[],[],2)[0]);c,_=self.listener.accept();return c
 def observe(self,c):L.observe_connection(c,os.getpgrp(),self.known,self.registered,self.kq,self.parent)
 def assert_exit(self,p,code,executed=False):
  self.assertEqual(p.wait(timeout=2),code)
  self.assertEqual(p.stdout.read(),b'EXECUTED\n' if executed else b'')
 def test_running_stopped_and_terminal_child_not_reaped(self):
  p=subprocess.Popen(['/bin/sleep','10'],start_new_session=True);self.children.append(p)
  self.assertEqual(self.parent(p.pid),os.getpid());self.assertEqual(self.terminal(p.pid),0)
  os.kill(p.pid,signal.SIGSTOP);time.sleep(.03);self.assertEqual(self.terminal(p.pid),0)
  os.kill(p.pid,signal.SIGCONT);os.kill(p.pid,signal.SIGKILL)
  end=time.monotonic()+2
  while self.terminal(p.pid)==0 and time.monotonic()<end:time.sleep(.01)
  self.assertEqual(self.terminal(p.pid),1);self.assertEqual(p.wait(timeout=2),-9)
 def test_actual_ack_exec_and_note_exit(self):
  p=self.spawn();self.observe(self.accept());self.assert_exit(p,0,True)
  L.drain_exits(self.kq,self.registered,self.events,2)
  self.assertEqual(self.events,[p.pid]);self.assertFalse(self.registered)
  self.assertTrue(self.known[p.pid]['acknowledged'])
 def test_disconnect_before_ack_never_executes(self):
  p=self.spawn();c=self.accept();c.close();self.assert_exit(p,114)
 def test_missing_ack_has_total_deadline(self):
  start=time.monotonic();p=self.spawn();c=self.accept()
  try:self.assert_exit(p,114);self.assertLess(time.monotonic()-start,1.5)
  finally:c.close()
 def test_wrong_server_peer_pid_never_sends_tuple(self):
  p=self.spawn(env={'VN_RENDER_OBSERVER_PID':str(os.getpid()+100000)});c=self.accept();c.settimeout(2)
  try:self.assertEqual(c.recv(12),b'');self.assert_exit(p,113)
  finally:c.close()
 def test_forged_client_tuple_rejected_without_adoption(self):
  c=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);c.connect(str(self.sock))
  c.sendall(struct.pack('!III',os.getpid()+100000,os.getpid(),os.getpgrp()))
  try:
   with self.assertRaisesRegex(ValueError,'peer_identity'):self.observe(self.accept())
   self.assertEqual(set(self.known),{os.getpid()});self.assertFalse(self.registered)
  finally:c.close()
 def test_unrelated_parent_rejected_without_adoption(self):
  code='import os,socket,struct,sys;s=socket.socket(socket.AF_UNIX);s.connect(sys.argv[1]);s.sendall(struct.pack("!III",os.getpid(),1,os.getpgrp()));sys.exit(0 if s.recv(1)==b"" else 2)'
  p=subprocess.Popen([sys.executable,'-c',code,str(self.sock)]);self.children.append(p)
  with self.assertRaisesRegex(ValueError,'parent_identity'):self.observe(self.accept())
  self.assertEqual(p.wait(timeout=2),0)
  self.assertEqual(set(self.known),{os.getpid()});self.assertFalse(self.registered)
 def test_wrong_group_child_exits_on_rejection_with_note_exit(self):
  p=self.spawn(start_new_session=True)
  with self.assertRaisesRegex(ValueError,'group_mismatch'):self.observe(self.accept())
  self.assertIn(p.pid,self.registered);self.assertFalse(self.known[p.pid]['acknowledged'])
  self.assert_exit(p,114);L.drain_exits(self.kq,self.registered,self.events,2)
  self.assertEqual(self.events,[p.pid]);self.assertFalse(self.registered)
if __name__=='__main__':unittest.main()
