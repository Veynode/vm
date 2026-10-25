#!/usr/bin/env python3
"""Real-host acceptance for the shared engine (docker CLI compatibility).

Isolated VEYNODEVM_HOME and DOCKER_CONFIG (the user's docker contexts are not
touched). Checks: engine start, docker context, docker run, docker build,
`veynodevm run --shared`, dynamic host publishing of `docker run -p`, compose
inside the engine, engine stop. Needs network access and the docker CLI.
Pass --dns when host resolvers listen only on loopback.
"""
import argparse, json, os, pathlib, shutil, signal, subprocess, sys, tempfile, time, urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
CLI = ROOT / 'target/debug/veynodevm'


def run(*args, env, check=True, timeout=600):
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True, env=env, timeout=timeout)
    if check and result.returncode != 0:
        raise SystemExit(f'{" ".join(map(str, args))} failed: {result.stderr.strip()[-800:]}')
    return result


def http(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            return urllib.request.urlopen(f'http://127.0.0.1:{port}/', timeout=3).read().decode()
        except OSError:
            time.sleep(0.5)
    return None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--dns', action='append', default=[])
    args = parser.parse_args()
    if not shutil.which('docker'):
        raise SystemExit('docker CLI not found')
    subprocess.run(['cargo', 'build', '--bins'], cwd=ROOT, check=True)
    home = pathlib.Path(tempfile.mkdtemp(prefix='veynodevm-engine-', dir=ROOT / '.local'))
    env = {**os.environ, 'VEYNODEVM_HOME': str(home), 'DOCKER_CONFIG': str(home / 'docker-config')}
    (home / 'docker-config').mkdir()
    subprocess.run([sys.executable, str(ROOT / 'scripts/install-dev-runtime.py')], env=env, check=True, stdout=subprocess.DEVNULL)
    daemon = subprocess.Popen([str(CLI), 'daemon', 'run'], env=env, stdout=open(home / 'daemon.log', 'w'), stderr=subprocess.STDOUT)
    docker = ['docker', '--context', 'veynodevm']
    summary = {}
    try:
        time.sleep(1)
        dns = [x for d in args.dns for x in ('--dns', d)]
        started = time.time()
        run(CLI, 'engine', 'start', '--port', '23860', *dns, env=env, timeout=1200)
        summary['engine_start_seconds'] = round(time.time() - started, 1)
        version = run(*docker, 'version', '--format', '{{.Server.Version}} {{.Server.Arch}}', env=env).stdout.strip()
        summary['server'] = version
        assert version.endswith('arm64'), version
        assert run(*docker, 'run', '--rm', 'alpine:3.20', 'echo', 'inner', env=env).stdout.strip() == 'inner'
        build = home / 'build'
        build.mkdir()
        (build / 'Dockerfile').write_text('FROM alpine:3.20\nRUN echo built > /built\nCMD ["cat","/built"]\n')
        run(*docker, 'build', '-q', '-t', 'veynodevm-engine-test', build, env=env)
        assert run(*docker, 'run', '--rm', 'veynodevm-engine-test', env=env).stdout.strip() == 'built'
        assert run(CLI, 'run', '--shared', '--rm', 'alpine:3.20', 'echo', 'shared', env=env).stdout.strip() == 'shared'
        run(*docker, 'run', '-d', '--rm', '--name', 'engine-web', '-p', '18792:80', 'nginx:alpine', env=env)
        page = http(18792)
        assert page and 'nginx' in page, 'inner published port must reach host loopback'
        run(*docker, 'stop', 'engine-web', env=env)
        time.sleep(4)
        assert http(18792, timeout=3) is None, 'port must be unpublished after the container stops'
        compose = run(CLI, 'exec', '--timeout', '30000', 'veynodevm-engine', 'docker', 'compose', 'version', '--short', env=env)
        summary['compose'] = compose.stdout.strip()
        run(CLI, 'engine', 'stop', env=env)
        summary['result'] = 'pass'
    finally:
        run(CLI, 'engine', 'rm', '-f', env=env, check=False)
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=20)
        except subprocess.TimeoutExpired:
            daemon.kill()
    shutil.rmtree(home, ignore_errors=True)
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
