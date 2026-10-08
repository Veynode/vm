#!/usr/bin/env python3
"""Actual local FD transfer/lifetime experiment; never a VM capture acceptance."""
import array,json,os,select,socket,struct
ID=struct.pack('=I',0x12345678)
def transferred_read(fd,peek):
 parent,child=socket.socketpair();pid=os.fork()
 if pid==0:
  parent.close()
  _,control,_,_=child.recvmsg(1,socket.CMSG_SPACE(array.array('i').itemsize))
  received=array.array('i');received.frombytes(control[0][2][:received.itemsize]);received=received[0]
  ready=select.poll();ready.register(received,select.POLLIN);events=ready.poll(0)
  result={'ready':events[0][1] if events else 0,'inode':os.fstat(received).st_ino}
  if events and events[0][1]&select.POLLIN:
   if peek:
    wrapped=socket.socket(fileno=received);data=wrapped.recv(4,socket.MSG_PEEK);wrapped.detach()
   else:data=os.read(received,4)
   result['data_hex']=data.hex()
  os.close(received);child.sendall(json.dumps(result).encode());child.close();os._exit(0)
 child.close();parent.sendmsg([b'F'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[fd]))]);result=json.loads(parent.recv(4096));parent.close();_,status=os.waitpid(pid,0);assert status==0;return result
pipe_read,pipe_write=os.pipe();os.write(pipe_write,ID)
legacy=[transferred_read(pipe_read,False),transferred_read(pipe_read,False)];os.close(pipe_read);os.close(pipe_write)
read_end,write_end=socket.socketpair(socket.AF_UNIX,socket.SOCK_STREAM);write_end.sendall(ID)
peek=[transferred_read(read_end.fileno(),True),transferred_read(read_end.fileno(),True)]
write_end.close();poll=select.poll();poll.register(read_end,select.POLLIN);after_close=poll.poll(100)
result={'scope':'OS IPC primitive proof only; no rebuilt QEMU, no IOSurface access, no VM acceptance','legacy_pipe':legacy,'experimental_socket_peek':peek,'socket_events_after_writer_close':after_close,'legacy_consumption_reproduced':legacy[0].get('data_hex')==ID.hex() and legacy[1]['ready']==0,'fresh_consumers_each_obtain_id':all(p.get('data_hex')==ID.hex() for p in peek),'writer_close_reports_hup':bool(after_close and after_close[0][1]&select.POLLHUP)}
read_end.close();print(json.dumps(result,indent=2));assert all(result[k] for k in ['legacy_consumption_reproduced','fresh_consumers_each_obtain_id','writer_close_reports_hup'])
