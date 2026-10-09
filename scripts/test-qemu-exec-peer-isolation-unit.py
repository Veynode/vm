#!/usr/bin/env python3
"""Offline per-incarnation oracles; no processes, signals, disks or VM starts."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('peer',Path(__file__).with_name('test-qemu-exec-peer-isolation.py'))
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)


def identity(vm, char):return {'vm':vm,'worker_instance':char*64,'device':1,'inode':ord(char)}
A,B=identity('a','a'),identity('b','b')
def scope(i):return {'vm':i['vm'],'worker_instance':i['worker_instance'],'owner_sha256':'c'*64,'epoch':1}
def active():return {'version':2,'launches':[A,B],'completed':[],'scopes':[scope(A),scope(B)]}
def stopped():return {'state':'stopped','exit_code':0,'execution_reconciliation_required':False}
def final():return {'version':2,'launches':[B],'completed':[A],'scopes':[scope(B)]}


class Tests(unittest.TestCase):
    def test_other_vm_changes_do_not_mask_or_invalidate_exact_projection(self):
        before=m.active_projection(active(),B)
        m.preserve_peer(before,final(),B)
        # Entire journal necessarily changed when A completes; B did not.
        self.assertNotEqual(active(),final())
        for key in ('scopes','launches'):
            bad=copy.deepcopy(final());bad[key]=[]
            with self.assertRaises(AssertionError):m.preserve_peer(before,bad,B)

    def test_same_vm_different_incarnation_does_not_satisfy_identity(self):
        bad=active();bad['launches'][0]=identity('a','d');bad['scopes'][0]=scope(bad['launches'][0])
        with self.assertRaises(AssertionError):m.active_projection(bad,A)
        bad=final();bad['completed']=[identity('a','d')]
        self.assertFalse(m.normal_proof_ready(stopped(),bad,A))

    def test_normal_status_and_durable_receipt_both_required(self):
        self.assertTrue(m.normal_proof_ready(stopped(),final(),A))
        pending=active();pending['scopes']=[scope(B)]
        self.assertFalse(m.normal_proof_ready(stopped(),pending,A))
        for change in ({'state':'running'},{'exit_code':1},{'exit_code':False},
                       {'execution_reconciliation_required':True},{'execution_recovery_required':True},
                       {'execution_reconciliation_status':'unconfirmed'}):
            self.assertFalse(m.normal_proof_ready({**stopped(),**change},final(),A))

    def test_reject_schema_change_duplicate_fields_and_unknown_journal(self):
        for value in ({**active(),'version':3},{**active(),'argv':['private']},
                      {**active(),'launches':[A,A,B]}):
            with self.assertRaises(AssertionError):m.projection(value,A)

    def test_running_peer_must_keep_all_three_process_incarnations(self):
        pins=[{'pid':10+i,'uid':501,'start_sec':5,'start_usec':1,'state':2,'ppid':9+i,'executable':'/fixture'} for i in range(3)]
        m.same_live_chain(pins,copy.deepcopy(pins))
        for key,value in [('start_usec',2),('state',4),('state',5),('ppid',1),('executable','/other')]:
            changed=copy.deepcopy(pins);changed[1][key]=value
            with self.assertRaises(AssertionError):m.same_live_chain(pins,changed)
        with self.assertRaises(AssertionError):m.same_live_chain(pins,[pins[0],None,pins[2]])

    def test_controller_release_has_direct_boolean_not_guest_envelope(self):
        m.release_acknowledged({'released':True})
        m.release_acknowledged({'released':True,'job_cancellation':{'confirmed':True}})
        for wrong in ({'result':{'released':True}}, {'released':1}, {'released':False}, {}, None):
            with self.assertRaises(AssertionError):m.release_acknowledged(wrong)

    def test_actual_mcp_text_unwrap_preserves_method_specific_shapes(self):
        # Exercise the existing MCP.tool parser, not a duplicate fake unwrap.
        import json
        client=object.__new__(m.base.MCP)
        for method, payload in (
            ('release_control', {'released':True}),
            ('exec_start', {'id':'request','result':{'job_id':'d'*32,'state':'running'}}),
            ('exec_cancel', {'id':'request','result':{'cancel_requested':True}}),
            ('exec_poll', {'id':'request','result':{'state':'finished','result':{'cancelled':True}}})):
            client.rpc=lambda _method, _params, value=payload: {'result':{'content':[{'type':'text','text':json.dumps(value)}]}}
            self.assertEqual(client.tool(method,{'id':'a'}),payload)

    def test_pre_cleanup_records_both_peers_before_any_info_query(self):
        events=[]; output={}
        vms={x:{'id':x,'started':True,'pins':[x]} for x in ['A','B']}
        def status(v):
            events.append('status-'+v['id'])
            if v['id']=='A':raise OSError('unavailable')
            return {'state':'running'}
        def chain(v):events.append('chain-'+v['id']);return v['pins']
        def journal():events.append('journal');return {'preserved':True}
        def info(v):events.append('info-'+v['id']);return {'result':'diagnostic'}
        m.capture_before_cleanup(vms,output,status,chain,journal,info,lambda:events.append('saved'))
        self.assertEqual(output['phase'],'before_any_force_stop')
        self.assertEqual(output['vms']['A']['status_error_type'],'OSError')
        self.assertEqual(output['vms']['A']['processes'],['A'])
        self.assertLess(events.index('chain-B'),events.index('info-A'))
        self.assertEqual([x for x in events if x.startswith('info-')],['info-A','info-B'])

    def test_bounded_read_only_info_real_pipe_success_and_timeout(self):
        import json,os,types,time
        for reply in ({'id':1,'result':{'content':[]}}, {'id':1,'error':{'code':-1}}):
            read,write=os.pipe()
            try:
                client=types.SimpleNamespace(sequence=0,buffer=bytearray(),p=types.SimpleNamespace(stdout=os.fdopen(read,'rb',buffering=0)))
                sent=[]
                def send(v):sent.append(v);os.write(write,json.dumps(reply).encode()+b'\n')
                client.send=send
                self.assertEqual(m.bounded_guest_info(client,'A',.1),reply)
                self.assertEqual(sent[0]['params'],{'name':'vm_guest_info','arguments':{'id':'A'}})
            finally:client.p.stdout.close();os.close(write)
        read,write=os.pipe()
        try:
            client=types.SimpleNamespace(sequence=0,buffer=bytearray(),p=types.SimpleNamespace(stdout=os.fdopen(read,'rb',buffering=0)),send=lambda v:None)
            start=time.monotonic()
            with self.assertRaises(AssertionError):m.bounded_guest_info(client,'A',.02)
            self.assertLess(time.monotonic()-start,.5)
        finally:client.p.stdout.close();os.close(write)

    def test_v3_must_be_explicit_and_strict_bounded_schema(self):
        value={**active(),'version':3}
        with self.assertRaises(AssertionError):m.projection(value,A)
        self.assertEqual(m.active_projection(value,A,3)['launches'],[A])
        self.assertTrue(m.normal_proof_ready(stopped(),{**final(),'version':3},A,3))
        for version in (1,4,True,'3'):
            with self.assertRaises(AssertionError):m.projection(value,A,version)
        for bad in ({**value,'version':2},{**value,'version':True},{**value,'unexpected':0}):
            with self.assertRaises(AssertionError):m.projection(bad,A,3)
        completed=[{'vm':'older','worker_instance':format(i,'064x'),'device':1,'inode':100+i} for i in range(1,66)]
        m.projection({**value,'completed':completed[:64]},A,3)
        with self.assertRaises(AssertionError):m.projection({**value,'completed':completed},A,3)
        too_many={**value,'launches':[{'vm':'pending','worker_instance':format(i,'064x'),'device':1,'inode':100+i} for i in range(1000,1257)],'scopes':[]}
        with self.assertRaises(AssertionError):m.projection(too_many,A,3)

    def test_v3_transition_fifo_and_peer_scope_preservation(self):
        receipts=[{'vm':'older','worker_instance':format(i,'064x'),'device':1,'inode':100+i} for i in range(1,65)]
        before={**active(),'version':3,'completed':receipts}
        after={'version':3,'launches':[B],'scopes':[scope(B)],'completed':receipts[1:]+[A]}
        m.witness.verify_transition(before,after,A,3)
        m.preserve_peer(m.active_projection(before,B,3),after,B,3)
        for completed in (receipts[:-1]+[A],list(reversed(receipts[1:]))+[A],receipts+[A]):
            with self.assertRaises(AssertionError):m.witness.verify_transition(before,{**after,'completed':completed},A,3)
        with self.assertRaises(AssertionError):m.preserve_peer(m.active_projection(before,B,3),{**after,'scopes':[]},B,3)

    def test_real_metadata_gc_requires_a_absence_and_b_pinned_file(self):
        import json,os,tempfile
        with tempfile.TemporaryDirectory() as temporary:
            state=Path(temporary);directory=state/'exec-journal';directory.mkdir(mode=0o700)
            identities=[]
            for original in (A,B):
                path=directory/(original['worker_instance']+'.witness')
                m.witness.private_write(path,json.dumps({'version':1,'vm':original['vm'],'worker_instance':original['worker_instance']}).encode())
                st=path.stat();identities.append({**original,'device':st.st_dev,'inode':st.st_ino})
            a,b=identities
            m.pending_witness_intact(state,b)
            self.assertFalse(m.completed_gc_ready(state,a))
            a_path=directory/(a['worker_instance']+'.witness');a_path.unlink()
            self.assertTrue(m.completed_gc_ready(state,a))
            m.pending_witness_intact(state,b)
            # Dangling symlink is not absence, and replacing pending B is unsafe.
            a_path.symlink_to('missing')
            self.assertFalse(m.completed_gc_ready(state,a))
            b_path=directory/(b['worker_instance']+'.witness');b_path.unlink();b_path.symlink_to('missing')
            with self.assertRaises(OSError):m.pending_witness_intact(state,b)
            a_path.unlink();b_path.unlink();directory.rmdir()
            with self.assertRaises(FileNotFoundError):m.completed_gc_ready(state,a)

    def test_progress_must_be_new_not_repeated_snapshot(self):
        before={'tick_bytes':3,'guest_elapsed_seconds':.6}
        m.increasing(before,{'tick_bytes':4,'guest_elapsed_seconds':.8})
        for after in (before,{'tick_bytes':4,'guest_elapsed_seconds':.6},{'tick_bytes':2,'guest_elapsed_seconds':.8}):
            with self.assertRaises(AssertionError):m.increasing(before,after)


if __name__=='__main__':unittest.main()
