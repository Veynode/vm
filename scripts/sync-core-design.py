#!/usr/bin/env python3
"""Copy core's design system (DESIGN.md and design-system/*.md) into docs/design/.

The layout mirrors core's, so links between the copied files keep working. A
relative link to anything else in core (its source, AGENTS.md, CONTRIBUTING.md)
is rewritten to the same path on GitHub; nothing else changes.

    scripts/sync-core-design.py [--core ../core]
"""
import argparse, pathlib, re, subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / 'docs/design'
GITHUB = 'https://github.com/Veynode/core/blob/main/'
LINK = re.compile(r'\]\(([^)#\s]+)(#[^)\s]*)?\)')


def rewrite(text, source, core, copied):
    def link(match):
        target, anchor = match.group(1), match.group(2) or ''
        if re.match(r'[a-z]+:', target):
            return match.group(0)
        resolved = (source.parent / target).resolve()
        if resolved in copied:
            return match.group(0)
        return f']({GITHUB}{resolved.relative_to(core).as_posix()}{anchor})'
    return LINK.sub(link, text)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--core', type=pathlib.Path, default=ROOT.parent / 'core')
    core = parser.parse_args().core.resolve()
    sources = [core / 'DESIGN.md', *sorted((core / 'design-system').glob('*.md'))]
    copied = set(sources)
    commit = subprocess.run(['git', '-C', str(core), 'rev-parse', '--short=9', 'HEAD'], capture_output=True, text=True, check=True).stdout.strip()
    for source in sources:
        out = OUT / source.relative_to(core)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(rewrite(source.read_text(), source, core, copied))
    (OUT / 'SOURCE.md').write_text(
        '# Design system, copied from core\n\n'
        f'[DESIGN.md](DESIGN.md) and [design-system/](design-system/) are a copy of the same files in [Veynode/core](https://github.com/Veynode/core) at commit `{commit}`, made by `scripts/sync-core-design.py`. '
        "Links to the rest of core point at GitHub; nothing else is changed. Don't edit them here: change them in core and run the script again.\n\n"
        "They are written for core's console. Here they govern `web/`, whose components are core's own ([web/src/vendor/veynode-ui](../../web/src/vendor/veynode-ui/SOURCE.md)); "
        "the parts about conversations, messages and bots have no counterpart in this project.\n"
    )
    print(f'{len(sources)} files from core {commit} into {OUT.relative_to(ROOT)}')


if __name__ == '__main__':
    main()
