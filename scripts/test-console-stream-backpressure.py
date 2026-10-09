#!/usr/bin/env python3
"""Real shim subprocess + Unix socket/pipe, synthetic bounded AU framing only.
No native encoder, VM, browser, TURN, network throttling or codec decode proof.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import tempfile
import threading
import time


def case(binary, mode):
    body = (b'\x01' * 64 if mode != 'newline-tail' else b'\x01\n' + b'\x01' * 62)
    if mode in {'slow-reader', 'slow-reader-no-stop-ack'}:
        body = b'\x01' * (1024 * 1024)
    expected = struct.pack('>IQB', len(body), 0, 1) + body
    events = []; errors = []; announced = threading.Event()
    with tempfile.TemporaryDirectory(prefix='vm-pipe-', dir='/tmp') as directory:
        path = str(Path(directory)/'broker.sock')
        listener = socket.socket(socket.AF_UNIX); listener.bind(path); listener.listen(1); listener.settimeout(3)
        def serve():
            try:
                connection, _ = listener.accept()
                with connection:
                    connection.settimeout(3)
                    reader = connection.makefile('rb')
                    def emit(kind, data):
                        connection.sendall(struct.pack('>IBQ', len(data), kind, 1)+data)
                    request = json.loads(reader.readline(2048)); assert request == {'request_id':'start','method':'start','codec':'h264'}
                    emit(1, json.dumps({'request_id':'start','result':{'epoch':1}}).encode())
                    emit(2, struct.pack('>QB', 0, 1)+body); announced.set()
                    request = json.loads(reader.readline(2048)); assert request == {'request_id':'stop','method':'stop'}
                    events.append('stop_received')
                    if mode != 'slow-reader-no-stop-ack':
                        emit(1, json.dumps({'request_id':'stop','result':{'state':'stopped'}}).encode()); events.append('stop_ack_sent')
            except Exception as error:
                errors.append(type(error).__name__)
            finally:
                announced.set()
        server = threading.Thread(target=serve, daemon=True); server.start()
        process = subprocess.Popen([str(binary),path,'--codec','h264'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        received = bytearray(); began = time.monotonic()
        try:
            if mode in {'small-no-newline', 'newline-tail'}:
                assert announced.wait(2), 'synthetic broker did not send frame'
                until = time.monotonic()+.4
                while len(received)<len(expected) and time.monotonic()<until:
                    if select.select([process.stdout],[],[],max(0,until-time.monotonic()))[0]:
                        part=os.read(process.stdout.fileno(),len(expected)-len(received))
                        if not part: break
                        received.extend(part)
                assert bytes(received)==expected, 'binary AU or trailing bytes withheld until a newline/later frame/exit'
                assert process.poll() is None, 'shim must still be running when the entire AU is readable'
                process.stdin.write(b'stop\n');process.stdin.flush()
            else:
                until = time.monotonic()+3
                while process.poll() is None and time.monotonic()<until:
                    if select.select([process.stdout],[],[],.01)[0]:
                        received.extend(os.read(process.stdout.fileno(),256))
                    time.sleep(.01)
                assert process.poll() is not None, 'slow reader extended record/cleanup deadline'
            output, stderr = process.communicate(timeout=3); received.extend(output)
            server.join(timeout=1)
            assert not server.is_alive() and not errors, 'synthetic broker did not finish cleanly'
            assert 'stop_received' in events
            if mode == 'slow-reader-no-stop-ack':
                assert process.returncode != 0 and b'stop unconfirmed' in stderr
            else:
                assert process.returncode == 0 and 'stop_ack_sent' in events
            if mode.startswith('slow-reader'):
                assert 0 < len(received) < len(expected), 'backpressure should leave a partial AU, never a false complete frame'
                if mode == 'slow-reader': assert b'media pipe write deadline' in stderr
            else:
                assert bytes(received)==expected
            return {'mode':mode,'passed':True,'received_bytes':len(received),'complete_record_bytes':len(expected),'duration_seconds':time.monotonic()-began,'child_exit_code':process.returncode,'stop_requested':True,'stop_ack_sent':'stop_ack_sent' in events,'scope':'synthetic AU; actual child, Unix socket and OS stdout pipe; zero exit attests stop ACK only'}
        finally:
            if process.poll() is None:
                process.kill(); process.wait(timeout=3)  # this still-owned test child only
            listener.close(); server.join(timeout=1)


def burst_case(binary, acknowledge=True):
    """Real shim queue overflow with synthetic AUs; zero exit attests only cleanup."""
    events = []; errors = []; began = time.monotonic()
    with tempfile.TemporaryDirectory(prefix='vm-burst-', dir='/tmp') as directory:
        path = str(Path(directory)/'broker.sock')
        listener = socket.socket(socket.AF_UNIX); listener.bind(path); listener.listen(1); listener.settimeout(3)
        def serve():
            try:
                connection, _ = listener.accept()
                with connection:
                    connection.settimeout(3)
                    reader = connection.makefile('rb')
                    def emit(kind, data):
                        connection.sendall(struct.pack('>IBQ',len(data),kind,1)+data)
                    assert json.loads(reader.readline(2048))['method']=='start'
                    emit(1,json.dumps({'request_id':'start','result':{'epoch':1}}).encode())
                    emit(2,struct.pack('>QB',0,1)+b'\x01'*(1024*1024))
                    for index in range(1,10):
                        emit(2,struct.pack('>QB',index*33333,0)+b'\x02'*64)
                        time.sleep(.02)
                    events.append('burst_sent')
                    request=json.loads(reader.readline(2048)); assert request=={'request_id':'stop','method':'stop'}
                    events.append('stop_received')
                    if acknowledge:
                        emit(1,json.dumps({'request_id':'stop','result':{'state':'stopped'}}).encode());events.append('stop_ack_sent')
            except Exception as error:
                errors.append(type(error).__name__)
        server=threading.Thread(target=serve,daemon=True);server.start()
        process=subprocess.Popen([str(binary),path,'--codec','h264'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            # Keep stdout blocked until exit: the burst overflows four events
            # before the 500ms whole-record deadline. No synthetic input stop.
            process.wait(timeout=4)
            output,stderr=process.communicate(timeout=1);server.join(timeout=1)
            passed=(not server.is_alive() and not errors and 'stop_received' in events and
                    ((process.returncode==0 and 'stop_ack_sent' in events) if acknowledge else
                     (process.returncode!=0 and 'stop_ack_sent' not in events)) and
                    0<len(output)<1024*1024+13)
            return {'mode':'burst-overflow' if acknowledge else 'burst-overflow-no-stop-ack',
                    'passed':passed,'child_exit_code':process.returncode,'received_bytes':len(output),
                    'duration_seconds':time.monotonic()-began,'stop_requested':'stop_received' in events,
                    'stop_ack_sent':'stop_ack_sent' in events,'broker_errors':errors,
                    'stderr':stderr.decode(errors='replace')[:1024],
                    'scope':'Synthetic AUs through actual shim; media failure needs independent stop acknowledgement'}
        finally:
            if process.poll() is None:process.kill();process.wait(timeout=3)
            listener.close();server.join(timeout=1)


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--burst-only',action='store_true');a=p.parse_args()
    binary=a.binary.resolve(strict=True);a.output.parent.mkdir(parents=True,exist_ok=True)
    report={'passed':False,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'checks':[],'scope':__doc__}
    fd=os.open(a.output,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
    with os.fdopen(fd,'w') as output:
        try:
            if not a.burst_only:
                for mode in ['small-no-newline','newline-tail','slow-reader','slow-reader-no-stop-ack']:
                    report['checks'].append(case(binary,mode))
            report['checks'].extend([burst_case(binary),burst_case(binary,False)])
            report['passed']=all(c['passed'] for c in report['checks'])
        except Exception as error:
            report['error']=str(error)
        finally:
            json.dump(report,output,indent=2);output.write('\n')
    print(json.dumps(report,indent=2));return 0 if report['passed'] else 1


if __name__=='__main__':raise SystemExit(main())
