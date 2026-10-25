#!/usr/bin/env python3
"""Run unmodified desktop fixture with an explicit independently built SPICE server."""
import importlib.util,os,pathlib,sys,json,hashlib,argparse
ROOT=pathlib.Path(__file__).resolve().parents[1]
BASE=ROOT/'.local/spice-reconnect-build/runtime';BASE.mkdir(exist_ok=True)
original=ROOT/'.local/runtime/utm'
for name in ['UTM.app','alpine-minirootfs-3.23.6-aarch64.tar.gz','spice-capture']:
 dest=BASE/name
 if not dest.exists():dest.symlink_to(original/name)
if not os.environ.get('VEYNODE_SPICE_LIBRARY'):raise SystemExit('Set VEYNODE_SPICE_LIBRARY to explicit independent test library')
spec=importlib.util.spec_from_file_location('desktop_fixture',ROOT/'scripts/qemu-linux-desktop-probe.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);module.BASE=BASE
result=module.main()
parser=argparse.ArgumentParser(add_help=False);parser.add_argument('--output',type=pathlib.Path,default=BASE/'linux-desktop-evidence.json');arguments,_=parser.parse_known_args()
if arguments.output.exists():
 report=json.loads(arguments.output.read_text());extra=pathlib.Path(str(arguments.output.with_suffix(''))+'-extra.reconnect.json')
 report['consumer_reconnect_tested_scope']='outer default capture-cycles only; extra consumer evidence is separate below'
 if extra.exists():
  details=json.loads(extra.read_text());report['extra_consumer_reconnect']={'tested':True,'passed':details['reconnect_passed'],'independent_processes':len(details['cycles']),'all_consumers_capture':details.get('all_consumers_capture',details.get('all_three_capture',False)),'changing_frame_counts':[next((e['capture_count'] for e in c['events'] if e.get('event')=='result'),0) for c in details['cycles']],'fd_count_stable_after_warmup':details.get('fd_count_stable_after_warmup'),'fault_injection':details.get('fault_injection'),'detail_file':str(extra)}
 report['selected_library_sha256']=hashlib.sha256(pathlib.Path(os.environ['VEYNODE_SPICE_LIBRARY']).read_bytes()).hexdigest()
 report['desktop_fixture_source_sha256']=hashlib.sha256((ROOT/'scripts/qemu-linux-desktop-probe.py').read_bytes()).hexdigest()
 arguments.output.write_text(json.dumps(report,indent=2)+'\n')
raise SystemExit(result)
