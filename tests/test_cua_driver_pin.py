import json
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
PIN = json.loads((ROOT / 'runtime/cua-driver.json').read_text())
DOCKERFILE = (ROOT / 'guest/images/ubuntu-desktop/Dockerfile').read_text()


class CuaDriverPin(unittest.TestCase):
    """The desktop image installs exactly the pinned cua-driver."""

    def test_assets_belong_to_the_pinned_release(self):
        self.assertRegex(PIN['commit'], r'^[0-9a-f]{40}$')
        for platform, asset in PIN['assets'].items():
            self.assertTrue(asset['url'].startswith(f"https://github.com/trycua/cua/releases/download/{PIN['tag']}/"), platform)
            self.assertIn(f"-{PIN['version']}-{platform}", asset['url'])
            self.assertTrue(asset['binary'].startswith(f"cua-driver-rs-{PIN['version']}-{platform}/"), platform)
            for key in ('sha256', 'binary_sha256'):
                self.assertRegex(asset[key], r'^[0-9a-f]{64}$', f'{platform} {key}')

    def test_ubuntu_desktop_checks_the_pinned_archive_and_binary(self):
        linux = PIN['assets']['linux-arm64']
        self.assertIn(f"ADD --checksum=sha256:{linux['sha256']} \\\n    {linux['url']} ", DOCKERFILE)
        self.assertIn(f"{linux['binary_sha256']}  /tmp/{linux['binary']}\" | sha256sum -c -", DOCKERFILE)
        # One version throughout: no stray release paths left from an earlier pin.
        versions = set(re.findall(r'cua-driver-rs-v?(\d+\.\d+\.\d+)', DOCKERFILE))
        self.assertEqual(versions, {PIN['version']})


if __name__ == '__main__':
    unittest.main()
