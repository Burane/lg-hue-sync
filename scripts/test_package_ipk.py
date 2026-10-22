import hashlib
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("package_ipk", Path(__file__).with_name("package_ipk.py"))
package_ipk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package_ipk)


def ar_entries(path):
    data = path.read_bytes()
    if not data.startswith(b"!<arch>\n"):
        raise AssertionError("not an ar archive")
    offset, entries = 8, {}
    while offset < len(data):
        header = data[offset:offset + 60]
        if len(header) != 60 or header[-2:] != b"`\n":
            raise AssertionError("invalid ar header")
        size = int(header[48:58])
        entries[header[:16].decode().strip().rstrip("/")] = data[offset + 60:offset + 60 + size]
        offset += 60 + size + size % 2
    return entries


class PackageIpkTests(unittest.TestCase):
    def test_real_rust_ares_package_preserves_launcher_and_manifest(self):
        self.assertIsNotNone(shutil.which("ares-rs-package"), "install ares-rs-package to verify packaging")
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            (output / "org.webosbrew.lg-hue-sync_99.0.0_all.ipk").write_bytes(b"stale")
            package, manifest_path = package_ipk.build_ipk(output_dir=output)
            info = json.loads((package_ipk.APP_DIR / "appinfo.json").read_text())
            self.assertEqual(package.name, f"{info['id']}_{info['version']}_all.ipk")
            archive = ar_entries(package)
            self.assertEqual(archive["debian-binary"], b"2.0\n")
            with tarfile.open(fileobj=io.BytesIO(archive["control.tar.gz"]), mode="r:gz") as control:
                member = next(m for m in control.getmembers() if Path(m.name).name == "control")
                body = control.extractfile(member).read().decode()
                self.assertIn(f"Package: {info['id']}\n", body)
                self.assertIn(f"Version: {info['version']}\n", body)
                self.assertIn("Architecture: all\n", body)
            with tarfile.open(fileobj=io.BytesIO(archive["data.tar.gz"]), mode="r:gz") as payload:
                prefix = f"usr/palm/applications/{info['id']}/"
                files = {m.name.removeprefix("./"): payload.extractfile(m).read() for m in payload.getmembers() if m.isfile()}
                expected = {prefix + p.relative_to(package_ipk.APP_DIR).as_posix(): p.read_bytes() for p in package_ipk.APP_DIR.rglob("*") if p.is_file()}
                package_info = f"usr/palm/packages/{info['id']}/packageinfo.json"
                self.assertEqual(set(files), set(expected) | {package_info})
                self.assertEqual({name: files[name] for name in expected}, expected)
                metadata = json.loads(files[package_info])
                self.assertEqual(metadata["id"], info["id"])
                self.assertEqual(metadata["version"], info["version"])
                self.assertFalse(any(Path(name).name == "config.json" for name in files))
            manifest = json.loads(manifest_path.read_text())
            for key in ("id", "version", "type", "title", "appDescription"):
                self.assertEqual(manifest[key], info[key])
            self.assertEqual(manifest["ipkUrl"], package.name)
            self.assertEqual(manifest["ipkSize"], package.stat().st_size)
            self.assertEqual(manifest["ipkHash"]["sha256"], hashlib.sha256(package.read_bytes()).hexdigest())
            self.assertTrue(manifest["rootRequired"])
            with patch.object(package_ipk, "build_ipk", return_value=(package, manifest_path)), patch.object(package_ipk.sys, "argv", ["package_ipk.py", "--print-path"]), patch("sys.stdout", new_callable=io.StringIO) as stdout:
                package_ipk.main()
                self.assertEqual(stdout.getvalue(), f"{package}\n")

    def test_packaging_errors_preserve_existing_outputs_and_reject_bad_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app, output = root / "app", root / "out"
            app.mkdir()
            output.mkdir()
            info = json.loads((package_ipk.APP_DIR / "appinfo.json").read_text())
            (app / "appinfo.json").write_text(json.dumps(info))
            package = output / f"{info['id']}_{info['version']}_all.ipk"
            manifest = output / f"{info['id']}.manifest.json"
            package.write_bytes(b"previous package")
            manifest.write_bytes(b"previous manifest")
            with patch.object(package_ipk.shutil, "which", return_value=None):
                with self.assertRaisesRegex(RuntimeError, "ares-rs-package is required"):
                    package_ipk.build_ipk(app, output)
            with patch.object(package_ipk.shutil, "which", return_value="ares-rs-package"), patch.object(package_ipk.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "ares-rs-package")):
                with self.assertRaises(subprocess.CalledProcessError):
                    package_ipk.build_ipk(app, output)
            with patch.object(package_ipk.shutil, "which", return_value="ares-rs-package"), patch.object(package_ipk.subprocess, "run"):
                with self.assertRaisesRegex(RuntimeError, "expected package"):
                    package_ipk.build_ipk(app, output)
            self.assertEqual(package.read_bytes(), b"previous package")
            self.assertEqual(manifest.read_bytes(), b"previous manifest")
            self.assertEqual(list(output.glob(".ares-package-*")), [])
            for key, value in (("id", "../escape"), ("version", "../escape"), ("title", None)):
                (app / "appinfo.json").write_text(json.dumps({**info, key: value}))
                with self.assertRaises(ValueError):
                    package_ipk.build_ipk(app, output)
            (app / "appinfo.json").write_text("[]")
            with self.assertRaisesRegex(ValueError, "JSON object"):
                package_ipk.build_ipk(app, output)


if __name__ == "__main__":
    unittest.main()
