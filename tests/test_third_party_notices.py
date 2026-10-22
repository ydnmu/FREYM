"""Static third-party notice and provenance checks; no network or build."""
import hashlib
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ThirdPartyNotices(unittest.TestCase):
    def test_notice_names_required_components(self):
        notice = (ROOT / "NOTICE").read_text(encoding="utf-8")
        for term in ("NVIDIA", "nvEncodeAPI.h", "GPUI", "Apache-2.0",
                     "Lucide", "ISC", "Geist", "OFL-1.1"):
            self.assertIn(term, notice)
        self.assertIn("Third-party components retain", notice)
        self.assertIn("original assets", notice)

    def test_license_texts_are_present(self):
        checks = {
            "LICENSES/GPUI-Apache-2.0.txt": ("Apache License", "Zed Industries"),
            "LICENSES/Lucide.txt": ("ISC License", "MIT License"),
            "crates/freym-ui/assets/fonts/ui-lab/Geist-LICENSE.txt": (
                "SIL OPEN FONT LICENSE", "The Geist Project Authors"),
        }
        for relative, markers in checks.items():
            text = (ROOT / relative).read_text(encoding="utf-8")
            for marker in markers:
                self.assertIn(marker, text, relative)

    def test_nvidia_header_matches_provenance(self):
        base = ROOT / "crates/frame-windows/vendor/nv-codec-headers"
        provenance = json.loads((base / "PROVENANCE.json").read_text(encoding="utf-8"))
        digest = hashlib.sha256((base / "nvEncodeAPI.h").read_bytes()).hexdigest()
        self.assertEqual(provenance["sha256"], digest)


if __name__ == "__main__":
    unittest.main()
