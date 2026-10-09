#!/usr/bin/env python3
"""Pure report/schema tests; no MCP, device queries or fixed Guest performance values."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec=importlib.util.spec_from_file_location('graphics_validator',Path(__file__).with_name('test-macos-graphics.py'))
graphics=importlib.util.module_from_spec(spec);spec.loader.exec_module(graphics)


def fixture(enabled=False):
    device={'schema':1,'name':'Synthetic GPU','architecture_name':'synthetic','registry_id':str(2**64-1),
        'os_version':'synthetic OS','family_support':{'apple5':True,'apple6':enabled,'metal3':enabled},
        'unified_memory':enabled,'max_threads_per_threadgroup_axes':{'width':128,'height':64,'depth':16},
        'max_threadgroup_memory_bytes':8192 if enabled else 16384,'max_buffer_bytes':2**31,
        'max_argument_buffer_sampler_count':128 if enabled else 8,
        'argument_buffers_support':{'raw_value':1 if enabled else 0,'tier':'tier2' if enabled else 'tier1'},
        'read_write_texture_support':{'raw_value':2 if enabled else 0,'tier':'tier2' if enabled else 'none'},
        'per_kernel_limits':None,'limit_semantics':'Device axes are not multiplied into a total.',
        'interpretation':'Capability is not proof of rendering.',**{key:enabled for key in graphics.FEATURES}}
    return {'os':'darwin','machine':'VirtualMacSynthetic','os_release':'synthetic OS','metal_device':'Synthetic GPU',
        'graphics':{'schema':1,'api':'metal','status':'reported','device_selection':'system-default',
            'evidence':'public-device-properties','queried_at_unix_ms':10,'device':device,
            'assessment':dict(graphics.ASSESSMENT),'scope':'Synthetic format fixture; no GPU evidence'},
        'username':'do-not-archive','file_policy':{'root':'do-not-archive'},'image':'do-not-archive'}


class GraphicsTests(unittest.TestCase):
    def test_different_legal_capabilities_and_privacy(self):
        for enabled in (False,True):
            result=graphics.validate_info(fixture(enabled))
            self.assertEqual(set(result),{'graphics','metal_device'})
            self.assertNotIn('do-not-archive',json.dumps(result))
            self.assertIs(result['graphics']['device']['supports_raytracing_compute'],enabled)
        value=fixture();device=value['graphics']['device']
        device['argument_buffers_support']={'raw_value':99,'tier':'unknown'}
        device['read_write_texture_support']={'raw_value':19,'tier':'unknown'}
        device['family_support']['apple20']=False
        graphics.validate_info(value)

    def test_bool_not_number_and_feature_types(self):
        for key,value in [('schema',True),('max_buffer_bytes',True),('max_threadgroup_memory_bytes',-1),
                          ('supports_raytracing_compute',0),('unified_memory','true'),('registry_id',2**64-1),
                          ('registry_id',str(2**64))]:
            info=fixture();info['graphics']['device'][key]=value
            with self.assertRaises(ValueError):graphics.validate_info(info)
        info=fixture();info['graphics']['queried_at_unix_ms']=True
        with self.assertRaises(ValueError):graphics.validate_info(info)

    def test_limits_tiers_and_claims_rejected(self):
        for change in ({'per_kernel_limits':{'max_threads':1024}},{'max_threads_total':1024},
                       {'max_threads_per_threadgroup_axes':{'width':128,'height':64,'depth':0}},
                       {'argument_buffers_support':{'raw_value':0,'tier':'tier2'}},
                       {'family_support':{'apple5':1}}):
            info=fixture();info['graphics']['device'].update(change)
            with self.assertRaises(ValueError):graphics.validate_info(info)
        info=fixture();info['graphics']['assessment']['performance']='fast'
        with self.assertRaises(ValueError):graphics.validate_info(info)

    def test_no_device_is_valid_but_not_probe_match(self):
        info=fixture();info['metal_device']='none'
        info['graphics'].update(status='unavailable',reason='no_metal_device',device=None)
        observation=graphics.validate_info(info)
        with self.assertRaisesRegex(ValueError,'requires_device'):
            graphics.compare_probe(observation,fixture()['graphics']['device'])
        info['metal_device']='invented'
        with self.assertRaises(ValueError):graphics.validate_info(info)

    def test_registry_and_time_do_not_mask_capability_change(self):
        observation=graphics.validate_info(fixture());probe=copy.deepcopy(observation['graphics']['device'])
        probe['registry_id']='7';observation['graphics']['queried_at_unix_ms']=30000
        result=graphics.compare_probe(observation,probe)
        self.assertTrue(result['properties_equal']);self.assertFalse(result['registry_ids']['equal'])
        probe['max_argument_buffer_sampler_count']+=1
        result=graphics.compare_probe(observation,probe)
        self.assertFalse(result['properties_equal'])
        self.assertEqual(result['different_property_names'],['max_argument_buffer_sampler_count'])

    def test_probe_only_completed_property_query(self):
        device=fixture()['graphics']['device'];device['host_os']=device.pop('os_version')
        event={'event':'completed','mode':'capabilities','capabilities_read':True,'capabilities':device,
               'pipeline_created':False,'library_created':False,'gpu_dispatch':False,'render_passed':False}
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'probe.log'
            path.write_text(json.dumps(event)+'\n')
            got,_=graphics.load_probe(path)
            self.assertEqual(got['os_version'],'synthetic OS');self.assertNotIn('host_os',got)
            for invalid in ({**event,'capabilities_read':False},{**event,'pipeline_created':True}):
                path.write_text(json.dumps(invalid)+'\n')
                with self.assertRaises(ValueError):graphics.load_probe(path)
            path.write_text(json.dumps(event)+'\n'+json.dumps({'event':'failed'})+'\n')
            with self.assertRaises(ValueError):graphics.load_probe(path)


if __name__=='__main__':unittest.main()
