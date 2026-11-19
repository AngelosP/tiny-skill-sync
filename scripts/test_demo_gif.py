import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from PIL import Image, ImageDraw

from demo_gif import encode


class DemoGifTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="tss-demo-encoder-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        image = Image.new("RGB", (640, 480), "white")
        ImageDraw.Draw(image).text((40, 40), "A disposable app window", fill="black")
        image.save(self.root / "shot.png")
        self.scenes = [
            dict(shot="shot.png", heading=f"Scene {i + 1}", caption="Only demo data.",
                 footer="A readable caption.", duration=4000, focus=[30, 30, 300, 80], tray=i == 5)
            for i in range(6)
        ]
        self.manifest = self.root / "story.json"
        self.manifest.write_text(json.dumps(self.scenes), encoding="utf-8")

    def test_decoded_contract_and_identical_bytes_on_repeat(self):
        first, second = self.root / "first.gif", self.root / "second.gif"
        encode(self.manifest, first)
        encode(self.manifest, second)
        self.assertEqual(hashlib.sha256(first.read_bytes()).digest(), hashlib.sha256(second.read_bytes()).digest())
        self.assertLess(first.stat().st_size, 5_000_000)
        with Image.open(first) as gif:
            self.assertEqual(gif.size, (960, 760))
            self.assertEqual(gif.n_frames, 24)
            self.assertEqual(gif.info["loop"], 0)
            duration = 0
            for index in range(gif.n_frames):
                gif.seek(index)
                duration += gif.info["duration"]
            self.assertEqual(duration, 24_000)

    def test_rejected_encoding_preserves_previous_output(self):
        output = self.root / "demo.gif"
        output.write_bytes(b"previous approved GIF")
        with patch("demo_gif.MAX_BYTES", 1), self.assertRaisesRegex(ValueError, "strictly smaller"):
            encode(self.manifest, output)
        self.assertEqual(output.read_bytes(), b"previous approved GIF")
        self.assertFalse(output.with_suffix(".tmp").exists())

    def test_refuses_a_capture_outside_its_folder(self):
        self.scenes[0]["shot"] = "..\\unrelated.png"
        self.manifest.write_text(json.dumps(self.scenes), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "beside the manifest"):
            encode(self.manifest, self.root / "demo.gif")


if __name__ == "__main__":
    unittest.main()
