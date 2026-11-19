"""Caption window-only demo captures. Never reads app data or captures the screen."""

import argparse
import json
import os
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

SIZE = (960, 760)
MAX_BYTES = 5_000_000


def font(size, bold=False):
    name = "segoeuib.ttf" if bold else "segoeui.ttf"
    return ImageFont.truetype(str(Path(os.environ["WINDIR"]) / "Fonts" / name), size)


def text(draw, xy, words, face, color, max_width=900):
    if draw.textbbox((0, 0), words, font=face)[2] > max_width:
        raise ValueError(f"Caption is too wide: {words!r}")
    draw.text(xy, words, font=face, fill=color)


def compose(scene, source, index, count, stroke):
    image = Image.new("RGB", SIZE, "#101820")
    draw = ImageDraw.Draw(image)
    text(draw, (24, 10), f"TINY SKILL SYNC  /  {index + 1:02d}", font(14, True), "#83c7ff")
    text(draw, (24, 31), scene["heading"], font(28, True), "#ffffff")
    text(draw, (24, 70), scene["caption"], font(19), "#e0e8ef")

    shot = source.copy()
    shot.thumbnail((936, 600), Image.Resampling.LANCZOS)
    x, y = (SIZE[0] - shot.width) // 2, 108 + (600 - shot.height) // 2
    image.paste(shot, (x, y))
    scale = shot.width / source.width
    left, top, right, bottom = scene["focus"]
    if not (0 <= left < right <= source.width and 0 <= top < bottom <= source.height):
        raise ValueError("The highlight is outside the captured window")
    box = (x + left * scale, y + top * scale, x + right * scale, y + bottom * scale)
    draw.rounded_rectangle(box, radius=5, outline="#ffbd59", width=stroke)

    if scene["tray"]:
        draw.ellipse((25, 719, 47, 741), fill="#2d7dd2")
    text(draw, (58 if scene["tray"] else 24, 716), scene["footer"], font(16), "#e0e8ef", 875)
    for step in range(count):
        start = 24 + step * 154
        draw.rectangle((start, 749, start + 143, 752), fill="#83c7ff" if step <= index else "#344453")
    return image


def encode(manifest, output):
    scenes = json.loads(manifest.read_text(encoding="utf-8-sig"))
    if len(scenes) != 6:
        raise ValueError("The demo needs six scenes")
    frames, durations = [], []
    for index, scene in enumerate(scenes):
        shot_path = manifest.parent / scene["shot"]
        if shot_path.resolve().parent != manifest.parent.resolve():
            raise ValueError("A shot must be beside the manifest")
        with Image.open(shot_path) as source:
            if source.width > 2000 or source.height > 1500:
                raise ValueError("Unexpected capture dimensions")
            for stroke, duration in zip((6, 4, 2, 3), (150, 150, 150, scene["duration"] - 450)):
                frames.append(compose(scene, source.convert("RGB"), index, len(scenes), stroke))
                durations.append(duration)
    if not 20_000 <= sum(durations) <= 30_000 or min(durations) <= 0:
        raise ValueError("The GIF must last 20-30 seconds")

    # One shared palette avoids color flicker between the pulse frames.
    samples = Image.new("RGB", (480 * len(scenes), 380))
    for index in range(len(scenes)):
        samples.paste(frames[index * 4].resize((480, 380)), (index * 480, 0))
    palette = samples.quantize(colors=256, method=Image.Quantize.MEDIANCUT)
    encoded = [frame.quantize(palette=palette, dither=Image.Dither.NONE) for frame in frames]
    output.parent.mkdir(parents=True, exist_ok=True)
    pending = output.with_suffix(".tmp")
    try:
        encoded[0].save(
            pending, format="GIF", save_all=True, append_images=encoded[1:],
            duration=durations, loop=0, optimize=False, disposal=1,
        )
        if pending.stat().st_size >= MAX_BYTES:
            raise ValueError("The GIF must be strictly smaller than 5,000,000 bytes")
        with Image.open(pending) as gif:
            if gif.size != SIZE or gif.n_frames != len(frames) or gif.info.get("loop") != 0:
                raise ValueError("The encoded GIF has the wrong shape or loop")
            elapsed = 0
            for index in range(gif.n_frames):
                gif.seek(index)
                elapsed += gif.info["duration"]
                gif.convert("RGB").save(manifest.parent / f"frame-{index:02d}.png")
            if elapsed != sum(durations):
                raise ValueError("The encoded GIF has the wrong duration")
        pending.replace(output)
    finally:
        pending.unlink(missing_ok=True)
    print(f"GIF: {SIZE[0]}x{SIZE[1]}, {len(frames)} frames, {sum(durations) / 1000:g} s, {output.stat().st_size} bytes.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    encode(args.manifest, args.output)
