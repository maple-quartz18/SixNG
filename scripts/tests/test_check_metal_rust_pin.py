import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "check_metal_rust_pin.py"
SPEC = importlib.util.spec_from_file_location("check_metal_rust_pin", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


MANIFEST = """\
[patch.crates-io]
metal-rust = { git = "https://github.com/maple-quartz18/Metal-Rust.git", rev = "e67e0fe6007f773a99debca0cfdb111cf49b792c" }
metal-rust-ffi = { git = "https://github.com/maple-quartz18/Metal-Rust.git", rev = "e67e0fe6007f773a99debca0cfdb111cf49b792c" }
"""

LOCKFILE = """\
[[package]]
name = "metal-rust"
version = "1.0.0"
source = "git+https://github.com/maple-quartz18/Metal-Rust.git?rev=e67e0fe6007f773a99debca0cfdb111cf49b792c#e67e0fe6007f773a99debca0cfdb111cf49b792c"

[[package]]
name = "metal-rust-ffi"
version = "1.0.0"
source = "git+https://github.com/maple-quartz18/Metal-Rust.git?rev=e67e0fe6007f773a99debca0cfdb111cf49b792c#e67e0fe6007f773a99debca0cfdb111cf49b792c"
"""


class CheckMetalRustPinTests(unittest.TestCase):
    def write_pair(self, manifest: str = MANIFEST, lockfile: str = LOCKFILE):
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        manifest_path = root / "Cargo.toml"
        lockfile_path = root / "Cargo.lock"
        manifest_path.write_text(manifest)
        lockfile_path.write_text(lockfile)
        return directory, manifest_path, lockfile_path

    def test_matching_pins_pass(self):
        directory, manifest_path, lockfile_path = self.write_pair()
        self.addCleanup(directory.cleanup)
        rev = MODULE.check_pins(manifest_path, lockfile_path)
        self.assertEqual(rev, "e67e0fe6007f773a99debca0cfdb111cf49b792c")

    def test_manifest_pins_must_match(self):
        manifest = MANIFEST.replace(
            'metal-rust-ffi = { git = "https://github.com/maple-quartz18/Metal-Rust.git", rev = "e67e0fe6007f773a99debca0cfdb111cf49b792c" }',
            'metal-rust-ffi = { git = "https://github.com/maple-quartz18/Metal-Rust.git", rev = "5b1aaca5786e82dd925f87058dcd89f6d6c885ac" }',
        )
        directory, manifest_path, lockfile_path = self.write_pair(manifest)
        self.addCleanup(directory.cleanup)
        with self.assertRaisesRegex(MODULE.PinMismatchError, "different revisions"):
            MODULE.check_pins(manifest_path, lockfile_path)

    def test_lockfile_pin_must_match_manifest(self):
        lockfile = LOCKFILE.replace(
            "?rev=e67e0fe6007f773a99debca0cfdb111cf49b792c",
            "?rev=5b1aaca5786e82dd925f87058dcd89f6d6c885ac",
        )
        directory, manifest_path, lockfile_path = self.write_pair(lockfile=lockfile)
        self.addCleanup(directory.cleanup)
        with self.assertRaisesRegex(MODULE.PinMismatchError, "Cargo.lock"):
            MODULE.check_pins(manifest_path, lockfile_path)

    def test_missing_lockfile_package_fails(self):
        lockfile = LOCKFILE
        lockfile = lockfile.replace(
            '''[[package]]
name = "metal-rust-ffi"
''',
            "",
        )
        directory, manifest_path, lockfile_path = self.write_pair(lockfile=lockfile)
        self.addCleanup(directory.cleanup)
        with self.assertRaisesRegex(MODULE.PinMismatchError, "missing package"):
            MODULE.check_pins(manifest_path, lockfile_path)

    def test_missing_manifest_patch_fails(self):
        manifest = "[workspace]\nresolver = \"2\"\n"
        directory, manifest_path, lockfile_path = self.write_pair(manifest)
        self.addCleanup(directory.cleanup)
        with self.assertRaisesRegex(MODULE.PinMismatchError, "patch"):
            MODULE.check_pins(manifest_path, lockfile_path)


if __name__ == "__main__":
    unittest.main()
