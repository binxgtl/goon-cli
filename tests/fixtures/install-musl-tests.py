"""Offline bootstrap contracts; subprocesses are mocked, never runs APT/dpkg."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("install_musl", Path(__file__).resolve().parents[2] / "scripts/install-musl.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class BootstrapTests(unittest.TestCase):
    def test_online_requires_strict_authenticated_update_and_no_download_install(self):
        calls = []
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            with patch.object(installer, "run", side_effect=lambda args, **kwargs: calls.append(args) or ""), \
                 patch.object(installer, "cache_packages") as save, patch.object(installer, "verify_installed") as verify:
                installer.install_online([], path, path, path, "amd64")
            self.assertIn("--error-on=any", calls[0])
            self.assertIn("Acquire::Retries=2", calls[0])
            self.assertIn("APT::Get::AllowUnauthenticated=false", calls[0])
            self.assertIn("Acquire::AllowInsecureRepositories=false", calls[0])
            self.assertIn("--download-only", calls[1])
            self.assertIn("--reinstall", calls[1])
            self.assertIn("--no-download", calls[3])
            save.assert_called_once()
            verify.assert_called_once()

    def test_root_owned_commands_have_root_owned_timeout(self):
        with patch.object(installer.subprocess, "run") as command:
            installer.run(["sudo", "apt-get", "update"], timeout=25)
        self.assertEqual(command.call_args.args[0][:5], ["sudo", "timeout", "--kill-after=5", "25", "apt-get"])
        self.assertEqual(command.call_args.kwargs["timeout"], 35)

    def test_signed_release_hashes_check_both_content_and_size(self):
        data = b"Package: musl\n"
        hashes = {"universe/binary-amd64/Packages": (hashlib.sha256(data).hexdigest(), len(data))}
        installer.check_index(data, hashes, "universe/binary-amd64/Packages")
        for invalid in (b"tampered", data + b"\n"):
            with self.assertRaises(ValueError):
                installer.check_index(invalid, hashes, "universe/binary-amd64/Packages")

    def test_gpgv_uses_installed_ubuntu_keys_not_cached_keys(self):
        def fake_run(args):
            self.assertEqual(args[:3], ["gpgv", "--keyring", installer.KEYRING])
            Path(args[4]).write_text("SHA256:\n abc123 12 universe/binary-arm64/Packages\n")
        with patch.object(installer, "run", side_effect=fake_run):
            self.assertEqual(installer.signed_hashes(Path("InRelease")), {"universe/binary-arm64/Packages": ("abc123", 12)})

    def make_cache(self, root):
        (root / "context.json").write_text(json.dumps({"arch": "amd64", "suite": "noble", "schema": 1}))
        bundle = root / "0"
        bundle.mkdir()
        (bundle / "InRelease").write_text("signed")
        (bundle / "metakey").write_text("universe/binary-amd64/Packages")
        records = []
        for name in installer.PACKAGES:
            data = (name + " package").encode()
            (root / (name + ".deb")).write_bytes(data)
            records.append(f"Package: {name}\nVersion: 1\nArchitecture: amd64\nSHA256: {hashlib.sha256(data).hexdigest()}\nSize: {len(data)}\n")
        index = "\n".join(records).encode()
        (bundle / "Packages").write_bytes(index)
        return {"universe/binary-amd64/Packages": (hashlib.sha256(index).hexdigest(), len(index))}

    def test_offline_verifies_signature_index_packages_and_installed_status(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            hashes = self.make_cache(root)
            commands = []
            with patch.object(installer, "signed_hashes", return_value=hashes), \
                 patch.object(installer, "package_identity", side_effect=lambda path: (path.stem, "1", "amd64")), \
                 patch.object(installer, "run", side_effect=lambda args: commands.append(args) or ""), \
                 patch.object(installer, "verify_installed") as verify:
                installer.install_cached(root, "amd64")
                self.assertIn("--refuse-downgrade", commands[0])
                self.assertNotIn("--force-depends", commands[0])
                verify.assert_called_once()
                commands.clear()
                (root / "musl.deb").write_bytes(b"tampered")
                with self.assertRaisesRegex(ValueError, "authenticated checksum"):
                    installer.install_cached(root, "amd64")
                self.assertFalse(commands, "must reject tampering before any installation")

    def test_online_cache_requires_signed_index_and_deb_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives = root / "archives"
            archives.mkdir()
            self.make_cache(archives)
            data = (archives / "0/Packages").read_bytes()
            (root / "ubuntu_InRelease").write_text("signed")
            hashes = {"universe/binary-amd64/Packages": (hashlib.sha256(data).hexdigest(), len(data))}
            cache = root / "cache"
            with patch.object(installer, "signed_hashes", return_value=hashes), \
                 patch.object(installer, "package_identity", side_effect=lambda path: (path.stem, "1", "amd64")), \
                 patch.object(installer.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, stdout=data)):
                installer.cache_packages(archives, root, "index|universe/binary-amd64/Packages", cache, "amd64")
                self.assertEqual(len(list(cache.glob("*.deb"))), 3)
                (archives / "musl.deb").write_bytes(b"tampered")
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    installer.cache_packages(archives, root, "index|universe/binary-amd64/Packages", cache, "amd64")
                self.assertNotEqual((cache / "musl.deb").read_bytes(), b"tampered", "failed validation must not replace a good cache")

    def test_bad_signature_and_architecture_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.make_cache(root)
            with patch.object(installer, "run") as command:
                with self.assertRaisesRegex(ValueError, "architecture"):
                    installer.install_cached(root, "arm64")
                command.assert_not_called()
            with patch.object(installer, "signed_hashes", side_effect=subprocess.CalledProcessError(1, "gpgv")), \
                 patch.object(installer, "run") as command:
                with self.assertRaises(subprocess.CalledProcessError):
                    installer.install_cached(root, "amd64")
                command.assert_not_called()

    def test_default_then_isolated_mirrors_then_cache(self):
        original_read = Path.read_text
        def read_text(path, *args, **kwargs):
            if str(path).replace("\\", "/").endswith("/etc/os-release"):
                return "VERSION_CODENAME=noble\n"
            return original_read(path, *args, **kwargs)
        with tempfile.TemporaryDirectory() as directory, \
             patch.dict(installer.os.environ, {"GOON_APT_CACHE": str(Path(directory) / "cache")}), \
             patch.object(Path, "read_text", read_text), \
             patch.object(installer, "run", return_value="amd64\n"), \
             patch.object(installer, "install_online", side_effect=ValueError("mirror unavailable")) as online, \
             patch.object(installer, "install_cached") as offline:
            installer.main()
            self.assertEqual(online.call_count, 1 + len(installer.MIRRORS["amd64"]))
            self.assertEqual(online.call_args_list[0].args[0], [])
            for call in online.call_args_list[1:]:
                self.assertIn("Dir::Etc::sourcelist=/dev/null", call.args[0])
                self.assertTrue(any(option.startswith("Dir::State::lists=") for option in call.args[0]))
            offline.assert_called_once()

    def test_apt_workspace_ignores_foreign_owned_cleanup_without_masking_errors(self):
        # Reproduce a runner's inaccessible _apt-owned partial directory.
        with tempfile.TemporaryDirectory() as directory:
            def inaccessible(path, *args, **kwargs):
                raise PermissionError("partial is owned by _apt")
            with patch.object(installer.tempfile, "mkdtemp", return_value=directory), \
                 patch.object(installer.shutil.os, "open", side_effect=inaccessible), \
                 patch.object(installer.shutil.os, "rmdir", side_effect=inaccessible):
                with installer.apt_workspace() as work:
                    self.assertEqual(work, directory)
                with self.assertRaisesRegex(ValueError, "install failed"):
                    with installer.apt_workspace():
                        raise ValueError("install failed")
            self.assertTrue(Path(directory).exists(), "ephemeral runner can reclaim inaccessible files")

    def test_uninstalled_package_is_not_success(self):
        with patch.object(installer, "run", return_value="unpacked"):
            with self.assertRaisesRegex(ValueError, "not installed"):
                installer.verify_installed()


if __name__ == "__main__":
    unittest.main()
