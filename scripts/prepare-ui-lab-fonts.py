"""Prepare GPUI reference fonts from the bundled OFL font sources.

Development-only: fonttools==4.60.1 and brotli==1.1.0. Normal Cargo builds use
the checked-in TTFs and do not require Python, pip or a network connection.
"""
from pathlib import Path
from tempfile import TemporaryDirectory
import hashlib
import json
import shutil

from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools.merge import Merger

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/freym-ui/assets/fonts/source"
OUTPUT = ROOT / "crates/freym-ui/assets/fonts/ui-lab"
OUTPUT.mkdir(parents=True, exist_ok=True)
manifest = {"inputs": {}, "outputs": {}, "weights": [400, 500, 600]}
for subset in ("latin", "latin-ext"):
    path = SOURCE / f"geist-{subset}-wght-normal.woff2"
    manifest["inputs"][path.name] = hashlib.sha256(path.read_bytes()).hexdigest()

with TemporaryDirectory() as temporary:
    for weight in manifest["weights"]:
        paths = []
        for subset in ("latin", "latin-ext"):
            font = TTFont(SOURCE / f"geist-{subset}-wght-normal.woff2")
            font = instantiateVariableFont(font, {"wght": weight}, inplace=True)
            font.flavor = None
            font.recalcTimestamp = False
            path = Path(temporary) / f"{subset}-{weight}.ttf"
            font.save(path)
            paths.append(str(path))
        merged = Merger().merge(paths)
        # A distinct family prevents collisions with an installed Geist version.
        style = {400: "Regular", 500: "Medium", 600: "SemiBold"}[weight]
        family = "Freym Lab Sans"
        for record in merged["name"].names:
            text = {1: family, 2: style, 3: f"{family} {style}",
                    4: f"{family} {style}", 6: f"FreymLabSans-{style}",
                    16: family, 17: style}.get(record.nameID)
            if text is not None:
                record.string = text.encode(record.getEncoding())
        merged["OS/2"].usWeightClass = weight
        destination = OUTPUT / f"FreymLabSans-{weight}.ttf"
        merged.recalcTimestamp = False
        merged.save(destination)
        check = TTFont(destination)
        # Include Turkish plus Latin; both original subsets must survive merging.
        assert set(map(ord, "FREYMçğıİöşüÇĞÖŞÜ")) <= set(check.getBestCmap())
        assert check["OS/2"].usWeightClass == weight
        manifest["outputs"][destination.name] = hashlib.sha256(destination.read_bytes()).hexdigest()

shutil.copyfile(SOURCE / "Geist-LICENSE.txt", OUTPUT / "Geist-LICENSE.txt")
(OUTPUT / "source.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
print(json.dumps(manifest, indent=2))
