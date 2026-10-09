#!/usr/bin/env python3
"""No VM/UI: narrow evidence parser and real owned MCP pipe test fixtures."""
import importlib.util
import json
from pathlib import Path
import tempfile
import time
import unittest
import subprocess

spec=importlib.util.spec_from_file_location('window_test',Path(__file__).with_name('test-qemu-native-window.py'))
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)

class Tests(unittest.TestCase):
    def test_status_is_display_only_and_gpu_counts_are_bounded(self):
        good={'native_window':True,'native_window_protocol':1,'display_only':True,'input_enabled':False,
              'window_open':True,'display_gpu_inflight':True,'display_gpu_inflight_limit':1,'drawables_submitted':5,'gpu_commands_completed':4,'gpu_commands_failed':0}
        module.window_status(good,True)
        for field,value in [('input_enabled',True),('display_only',False),('window_open',False),('display_gpu_inflight',1),('display_gpu_inflight',0),('display_gpu_inflight_limit',True),('display_gpu_inflight_limit',2),('gpu_commands_completed',6),('gpu_commands_completed',True)]:
            with self.assertRaises(AssertionError):module.window_status(good|{field:value},True)

    def test_pacing_contract_is_not_measured_fps(self):
        caps={'capture_limit_fps':60,'native_window_capture_limit_fps':60,'background_capture_limit_fps':30,
              'encoder_limit_fps':30,'idle_encode_interval_ms':1000,'display_refresh_preference_hz':60,'capture_current_limit_fps':30}
        module.pacing_limits(caps)
        for field,value in [('encoder_limit_fps',True),('encoder_limit_fps',30.0),('encoder_limit_fps',60),('capture_current_limit_fps',45)]:
            with self.assertRaises(AssertionError):module.pacing_limits(caps|{field:value})
        del caps['idle_encode_interval_ms']
        with self.assertRaises(AssertionError):module.pacing_limits(caps)

    def test_png_must_decode_and_match_dimensions(self):
        png=subprocess.check_output(['ffmpeg','-hide_banner','-loglevel','error','-f','lavfi','-i','color=c=red:s=16x16','-frames:v','1','-f','image2pipe','-vcodec','png','pipe:1'],timeout=15)
        result=module.decode_png(png,{'width':16,'height':16,'byte_length':len(png)})
        self.assertEqual(result['decoded_frames'],1)
        for data,meta in [(png,{'width':18,'height':16,'byte_length':len(png)}),(png[:20],{'width':16,'height':16,'byte_length':20})]:
            with self.assertRaises(AssertionError):module.decode_png(data,meta)

    def test_observe_denial_and_client_cleanup_use_actual_owned_child(self):
        with tempfile.TemporaryDirectory() as directory:
            executable=Path(directory)/'mcp-fixture';executable.write_text('''#!/usr/bin/env python3
import sys,json
for line in sys.stdin:
 q=json.loads(line)
 if 'id' not in q:continue
 r={'jsonrpc':'2.0','id':q['id']}
 if q['method']=='initialize':r['result']={'protocolVersion':'2025-06-18'}
 else:r['error']={'code':-32602,'message':'Tool unknown or permission not granted'}
 print(json.dumps(r),flush=True)
''');executable.chmod(0o700)
            client=module.MCP(executable,Path(directory),1)
            self.assertTrue(client.tool('vm_show_window','fixture',denied=True)['denied'])
            with self.assertRaises(AssertionError):client.tool('vm_start','fixture')
            client.close();self.assertEqual(client.process.returncode,0)

    def test_stalled_owned_client_is_bounded_and_reaped(self):
        with tempfile.TemporaryDirectory() as directory:
            executable=Path(directory)/'mcp-stall'
            executable.write_text('#!/usr/bin/env python3\nimport time\ntime.sleep(20)\n');executable.chmod(0o700)
            from unittest.mock import patch
            original=subprocess.Popen;owned=[]
            def spawn(*args,**kwargs):
                child=original(*args,**kwargs);owned.append(child);return child
            began=time.monotonic()
            with patch.object(module.subprocess,'Popen',side_effect=spawn):
                with self.assertRaises(TimeoutError):module.MCP(executable,Path(directory),.1)
            self.assertLess(time.monotonic()-began,3)
            self.assertEqual(len(owned),1);self.assertIsNotNone(owned[0].returncode)
            # The exact owned child has been reaped, even if startup had not yet
            # reached the fixture's Python body when its deadline expired.
            import os
            with self.assertRaises(ChildProcessError):os.waitpid(owned[0].pid,os.WNOHANG)


if __name__=='__main__':unittest.main()
