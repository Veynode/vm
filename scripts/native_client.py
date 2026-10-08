"""Bounded test/provisioning client for the real native JSON worker, not a mock."""
import json,queue,subprocess,threading,time
class NativeClient:
 def __init__(self,binary,config,stderr=None):
  self.process=subprocess.Popen([str(binary),'run','--config',str(config)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr or subprocess.DEVNULL,text=True,bufsize=1)
  self.messages=queue.Queue();self.sequence=0
  def read():
   try:
    for line in self.process.stdout:self.messages.put(json.loads(line))
   finally:self.messages.put(None)
  threading.Thread(target=read,daemon=True).start()
 def call(self,method,timeout=40,**payload):
  self.sequence+=1;sequence=self.sequence
  self.process.stdin.write(json.dumps({'id':sequence,'method':method,**payload})+'\n');self.process.stdin.flush()
  deadline=time.monotonic()+timeout
  while time.monotonic()<deadline:
   item=self.messages.get(timeout=max(.001,deadline-time.monotonic()))
   if item is None:raise RuntimeError('native worker exited')
   if item.get('event')=='state' and item.get('state')=='error':raise RuntimeError(item)
   if item.get('id')!=sequence:continue
   if 'error' in item:raise RuntimeError(item['error'])
   return item['result']
  raise TimeoutError(method)
 def guest(self,method,**payload):
  response=self.call('guest_request',request={'id':str(self.sequence+1),'method':method,**payload})
  if 'error' in response:raise RuntimeError(response['error'])
  return response['result']
 def ready(self):
  for _ in range(100):
   try:return self.guest('info')
   except (RuntimeError,queue.Empty):
    if self.process.poll() is not None:raise
    time.sleep(.1)
  raise TimeoutError('guest not ready')
 def close(self):
  if self.process.poll() is None:
   try:self.call('stop',timeout=8)
   except Exception:pass
   self.process.stdin.close()
   try:self.process.wait(timeout=8)
   except subprocess.TimeoutExpired:self.process.kill();self.process.wait()
 def __enter__(self):return self
 def __exit__(self,*_):self.close()
