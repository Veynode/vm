#!/usr/bin/env python3
"""Offline journal and receipt oracles; never queries processes or starts a VM."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

spec=importlib.util.spec_from_file_location('witness',Path(__file__).with_name('test-qemu-exec-witness-recovery.py'))
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)


def identity(char='a'):
    return {'vm':'test','worker_instance':char*64,'device':1,'inode':ord(char)}


def journal():
    return {'version':2,'scopes':[{'vm':'test','worker_instance':'a'*64,'owner_sha256':'b'*64,'epoch':1}],
        'launches':[identity()],'completed':[]}


class Tests(unittest.TestCase):
    def test_v3_requires_explicit_selection_and_keeps_v2_strict(self):
        value={**journal(),'version':3};raw=json.dumps(value).encode()
        self.assertEqual(m.validate_journal(raw,3),value)
        with self.assertRaises(AssertionError):m.validate_journal(raw)
        with self.assertRaises(AssertionError):m.validate_journal(json.dumps(journal()).encode(),3)
        for version in [1,4,True,'3']:
            with self.assertRaises(AssertionError):m.validate_journal(raw,version)
        padded=raw+b' '*(131073-len(raw))
        self.assertEqual(m.validate_journal(padded,3),value)
        with self.assertRaises(AssertionError):m.validate_journal(padded)
        with self.assertRaises(AssertionError):m.validate_journal(raw+b' '*262144,3)
        with tempfile.TemporaryDirectory() as name:
            state=Path(name);d=state/'exec-journal';d.mkdir(mode=0o700)
            m.private_write(d/'exec-scopes.json',padded)
            m.private_write(d/'exec-scopes.required',b'v1\n')
            self.assertEqual(m.read_journal(state,3)[0],value)
            with self.assertRaises(AssertionError):m.read_journal(state)

    def test_v3_independent_capacity_and_conflicting_records(self):
        def ident(n):return {'vm':'vm'+str(n),'worker_instance':format(n,'064x'),'device':1,'inode':n+1}
        value={'version':3,'scopes':[],'launches':[ident(n) for n in range(256)],'completed':[ident(n) for n in range(256,320)]}
        self.assertEqual(m.validate_journal(json.dumps(value).encode(),3),value)
        for key in ['launches','completed']:
            bad=copy.deepcopy(value);bad[key].append(ident(320))
            with self.assertRaises(AssertionError):m.validate_journal(json.dumps(bad).encode(),3)
        bad=copy.deepcopy(value);bad['completed'][0]=bad['launches'][0]
        with self.assertRaises(AssertionError):m.validate_journal(json.dumps(bad).encode(),3)
        with self.assertRaises(AssertionError):m.validate_journal(b'{"version":3,"version":3,"scopes":[],"launches":[],"completed":[]}',3)

    def test_v3_transition_order_and_gc_requires_actual_absence(self):
        before={**journal(),'version':3,'completed':[identity('c')]}
        after={'version':3,'scopes':[],'launches':[],'completed':[identity('c'),identity()]}
        m.verify_transition(before,after,identity(),3)
        bad=copy.deepcopy(after);bad['completed'].reverse()
        with self.assertRaises(AssertionError):m.verify_transition(before,bad,identity(),3)
        with tempfile.TemporaryDirectory() as name:
            state=Path(name)
            with self.assertRaises(FileNotFoundError):m.require_completed_gc(state,identity())
            d=state/'exec-journal';d.mkdir(mode=0o700)
            m.require_completed_gc(state,identity())
            path=d/('a'*64+'.witness');m.private_write(path,b'retained')
            with self.assertRaises(AssertionError):m.require_completed_gc(state,identity())
            path.unlink();path.symlink_to('missing')
            with self.assertRaises(AssertionError):m.require_completed_gc(state,identity())

    def test_valid_v2_and_marker_version_distinct(self):
        value=journal();self.assertEqual(m.validate_journal(json.dumps(value).encode()),value)
        with tempfile.TemporaryDirectory() as name:
            state=Path(name);d=state/'exec-journal';d.mkdir(mode=0o700)
            m.private_write(d/'exec-scopes.json',json.dumps(value).encode())
            m.private_write(d/'exec-scopes.required',b'v1\n')
            self.assertEqual(m.read_journal(state)[0],value)

    def test_legacy_unknown_duplicate_and_sensitive_fields_refused(self):
        variants=[{'version':1,'scopes':[]}, {**journal(),'argv':['secret']}]
        x=journal();x['completed']=[identity()];variants.append(x)
        x=journal();x['scopes'][0]['owner']='c'*64;variants.append(x)
        x=journal();x['launches'][0]['inode']=True;variants.append(x)
        x=journal();x['launches']=[];variants.append(x)
        for value in variants:
            with self.assertRaises(AssertionError):m.validate_journal(json.dumps(value).encode())
        with self.assertRaises(AssertionError):m.validate_journal(b'{"version":2,"version":2,"scopes":[],"launches":[],"completed":[]}')

    def test_exact_transition_preserves_other_incarnation(self):
        before=journal();before['launches'].append(identity('c'))
        before['scopes'].append({'vm':'test','worker_instance':'c'*64,'owner_sha256':'d'*64,'epoch':2})
        after={'version':2,'scopes':[before['scopes'][1]],'launches':[identity('c')],'completed':[identity()]}
        m.verify_transition(before,after,identity())
        for key in ['scopes','launches','completed']:
            bad=copy.deepcopy(after);bad[key]=[]
            with self.assertRaises(AssertionError):m.verify_transition(before,bad,identity())

    def test_old_receipt_does_not_allow_new_scope_deletion(self):
        current={'version':2,'scopes':[{'vm':'test','worker_instance':'c'*64,'owner_sha256':'d'*64,'epoch':1}],
            'launches':[identity('c')],'completed':[identity()]}
        m.require_retained(current,copy.deepcopy(current))
        for key in ['scopes','launches','completed']:
            bad=copy.deepcopy(current);bad[key]=[]
            with self.assertRaises(AssertionError):m.require_retained(current,bad)

    def test_receipt_exact_basis_instance_and_real_bool(self):
        good={'reconciled':True,'basis':'original_incarnation_witness_exclusive','id':'test','worker_instance':'a'*64}
        m.verify_receipt(good,identity())
        for key,value in [('reconciled',1),('basis','guest_ack'),('worker_instance','c'*64),('extra',True)]:
            bad={**good,key:value}
            with self.assertRaises(AssertionError):m.verify_receipt(bad,identity())

    def test_real_witness_read_rejects_wrong_inode_symlink_and_false_startup_proof(self):
        with tempfile.TemporaryDirectory() as name:
            state=Path(name);d=state/'exec-journal';d.mkdir(mode=0o700);path=d/('a'*64+'.witness')
            m.private_write(path,json.dumps({'version':1,'vm':'test','worker_instance':'a'*64}).encode())
            st=path.stat();i={**identity(),'device':st.st_dev,'inode':st.st_ino}
            runtime={'execution_witness':{**i,'qmp_fdsets_verified_at_start':True}}
            self.assertEqual(m.witness_from_runtime(runtime,'test','a'*64,state),i)
            for bad in [{**i,'inode':st.st_ino+1,'qmp_fdsets_verified_at_start':True},{**i,'qmp_fdsets_verified_at_start':1}]:
                with self.assertRaises(AssertionError):m.witness_from_runtime({'execution_witness':bad},'test','a'*64,state)
            path.rename(d/'original');path.symlink_to('original')
            with self.assertRaises(OSError):m.witness_from_runtime(runtime,'test','a'*64,state)


if __name__=='__main__':unittest.main()
