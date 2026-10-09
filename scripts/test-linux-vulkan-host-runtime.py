#!/usr/bin/env python3
"""Offline source/patch boundary tests; no VM, dynamic loader or GPU."""
import hashlib
import importlib.util
from pathlib import Path
import tarfile
import subprocess
import tempfile
import unittest
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('host_runtime', ROOT/'scripts/linux-vulkan-build-host-runtime.py')
B = importlib.util.module_from_spec(spec); spec.loader.exec_module(B)
class PatchBoundary(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        path=ROOT/'.local/linux-vulkan-source-review/virglrenderer.tar.gz'
        assert B.sha(path)==B.INPUTS[path.name]
        with tarfile.open(path) as tar:
            name=next(n for n in tar.getnames() if n.endswith('/src/proxy/proxy_server.c'))
            cls.source=tar.extractfile(name).read()
    def test_exact_source_and_only_apple_group_change(self):
        result=B.patch_proxy(self.source)
        original=self.source.decode();patched=result.decode()
        start=original.index('static pid_t\nproxy_server_spawn(')
        end=original.index('#endif /* __APPLE__ */',start)
        pstart=patched.index('static pid_t\nproxy_server_spawn(')
        pend=patched.index('#endif /* __APPLE__ */',pstart)
        # Restore the two removed group directives; the function must otherwise match.
        function=patched[pstart:pend].replace('POSIX_SPAWN_CLOEXEC_DEFAULT);','POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETPGROUP);\n   posix_spawnattr_setpgroup(&attr, 0);')
        self.assertEqual(function,original[start:end])
        self.assertEqual(patched[pend:],original[end:])
    def test_changed_source_refused(self):
        for bad in [self.source+b'\n',self.source.replace(b'posix_spawnattr_setpgroup',b'wrong_spawnattr_setpgroup')]:
            with self.assertRaisesRegex(ValueError,'source pin'):B.patch_proxy(bad)
    def test_double_patch_refused(self):
        with self.assertRaisesRegex(ValueError,'source pin'):B.patch_proxy(B.patch_proxy(self.source))
    def test_real_spawn_group_before_and_after(self):
        # Extract the exact changed upstream function. No renderer/Metal code is loaded.
        for patched in [False, True]:
            source=(B.patch_proxy(self.source) if patched else self.source).decode()
            function=source[source.index('static pid_t\nproxy_server_spawn('):source.index('#endif /* __APPLE__ */',source.index('static pid_t\nproxy_server_spawn('))]
            headers='#include <spawn.h>\n#include <crt_externs.h>\n#include <stdio.h>\n#include <string.h>\n#include <unistd.h>\n#include <signal.h>\n#include <sys/wait.h>\n#define proxy_log(...) fprintf(stderr,__VA_ARGS__)\n'
            body='int main(void) { int fds[2]; if(pipe(fds)) return 2; char *args[]={"/bin/sleep","10",NULL}; pid_t p=proxy_server_spawn(args[0],args,fds[1]); if(p<=0)return 3; int same=getpgid(p)==getpgrp(); kill(p,SIGKILL); int status=0; if(waitpid(p,&status,0)!=p||!WIFSIGNALED(status)||WTERMSIG(status)!=SIGKILL)return 4; printf("same_group=%d reaped=1\\n",same); return 0; }'
            with tempfile.TemporaryDirectory() as d:
                path=Path(d);(path/'test.c').write_text(headers+function+body)
                subprocess.run(['clang','-Wall','-Wextra','-Werror',str(path/'test.c'),'-o',str(path/'probe')],check=True,capture_output=True)
                r=subprocess.run([str(path/'probe')],start_new_session=True,check=True,capture_output=True,text=True,timeout=3)
                self.assertEqual(r.stdout,f'same_group={int(patched)} reaped=1\n')
    def test_vendor_inputs_pinned(self):
        fw=ROOT/'.local/windows-gpu/UTM.app/Contents/Frameworks'
        for name,digest in B.FRAMEWORKS.items():
            self.assertEqual(B.sha(fw/(name+'.framework')/'Versions/A'/name),digest)
if __name__=='__main__':unittest.main()
