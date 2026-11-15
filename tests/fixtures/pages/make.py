"""Writes the shared page-reading test set: made-up pages with known words.

Run with any Python that has Pillow (`uv run python make.py`). Readers are checked against
them as docs/consumers.md (Page readings) describes. Each <name> gets <name>.json: per page the frame
shown (points for PDFs, pixels for images) and the words with their boxes in that frame, 0-1
from the top left.
"""

import io
import json
import zlib
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).parent
FONT = "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf"
LINES = [
    "Werkbon 4271 voor pomp onderhoud",
    "Geleverd op dinsdag door monteur Rekzil",
    "Afdichting vervangen en lager gecontroleerd",
    "Volgende inspectie over twaalf maanden",
]


def turn(u, v, rotate):
    return {90: (1 - v, u), 180: (1 - u, 1 - v), 270: (v, 1 - u)}.get(rotate, (u, v))


def turned(box, rotate):
    a, b = turn(box[0], box[1], rotate), turn(box[2], box[3], rotate)
    return [min(a[0], b[0]), min(a[1], b[1]), max(a[0], b[0]), max(a[1], b[1])]


def drawn(width, height, lines=LINES, size=None, top=0.12):
    """An upright scan with the lines on it, and each word's box in 0-1."""
    image = Image.new("L", (width, height), 255)
    draw = ImageDraw.Draw(image)
    size = size or round(width / 28)
    font = ImageFont.truetype(FONT, size)
    words = []
    y = top * height
    for line in lines:
        x = 0.08 * width
        for text in line.split():
            x0, y0, x1, y1 = draw.textbbox((x, y), text, font=font)
            draw.text((x, y), text, font=font, fill=0)
            words.append([text, x0 / width, y0 / height, x1 / width, y1 / height])
            x = x1 + font.getlength(" ")
        y += size * 1.8
    return image, words


def jpeg(image):
    out = io.BytesIO()
    image.convert("RGB").save(out, "JPEG", quality=92)
    return out.getvalue()


def layer(lines, media, start, size=14):
    """Courier text in PDF space; words with boxes in the media box, 0-1 from the top left."""
    x0, y0, x1, y1 = media
    ops, words = [], []
    y = start[1]
    for line in lines:
        x = start[0]
        for text in line.split():
            ops.append(f"BT /F1 {size} Tf {x:.2f} {y:.2f} Td ({text}) Tj ET")
            right = x + 0.6 * size * len(text)
            box = [x, y + 0.63 * size, right, y - 0.15 * size]
            words.append(
                [
                    text,
                    (box[0] - x0) / (x1 - x0),
                    1 - (box[1] - y0) / (y1 - y0),
                    (box[2] - x0) / (x1 - x0),
                    1 - (box[3] - y0) / (y1 - y0),
                ]
            )
            x = right + 0.6 * size
        y -= size * 1.6
    return "\n".join(ops), words


def pdf(pages):
    objects = []

    def add(body):
        objects.append(body)
        return len(objects)

    add(b"")
    add(b"")
    font = add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>")
    kids = []
    for page in pages:
        names = []
        for number, (data, width, height) in enumerate(page.get("images", [])):
            ref = add(
                f"<< /Type /XObject /Subtype /Image /Width {width} /Height {height} "
                f"/ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode "
                f"/Length {len(data)} >>\nstream\n".encode()
                + data
                + b"\nendstream"
            )
            names.append(f"/Im{number} {ref} 0 R")
        content = zlib.compress(page["content"].encode())
        stream = add(
            f"<< /Length {len(content)} /Filter /FlateDecode >>\nstream\n".encode()
            + content
            + b"\nendstream"
        )
        extra = ""
        if page.get("crop"):
            extra += " /CropBox [{} {} {} {}]".format(*page["crop"])
        if page.get("rotate"):
            extra += f" /Rotate {page['rotate']}"
        kids.append(
            add(
                "<< /Type /Page /Parent 2 0 R /MediaBox [{} {} {} {}]{} /Contents {} 0 R "
                "/Resources << /Font << /F1 {} 0 R >> /XObject << {} >> >> >>".format(
                    *page["media"], extra, stream, font, " ".join(names)
                ).encode()
            )
        )
    objects[0] = b"<< /Type /Catalog /Pages 2 0 R >>"
    objects[1] = (
        f"<< /Type /Pages /Count {len(kids)} /Kids [{' '.join(f'{k} 0 R' for k in kids)}] >>"
    ).encode()
    out = io.BytesIO()
    out.write(b"%PDF-1.7\n")
    offsets = []
    for number, body in enumerate(objects, 1):
        offsets.append(out.tell())
        out.write(f"{number} 0 obj\n".encode() + body + b"\nendobj\n")
    xref = out.tell()
    out.write(f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode())
    for offset in offsets:
        out.write(f"{offset:010d} 00000 n \n".encode())
    out.write(f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode())
    return out.getvalue()


def rounded(words):
    return [[text, *(round(value, 4) for value in box)] for text, *box in words]


def save(name, data, pages):
    (HERE / name).write_bytes(data)
    expected = [
        {"width": width, "height": height, "method": method, "words": rounded(words)}
        for width, height, method, words in pages
    ]
    (HERE / f"{name}.json").write_text(json.dumps({"pages": expected}, indent=1) + "\n")


def scan_page(image, media, rotate=0):
    width, height = image.size
    w, h = media[2] - media[0], media[3] - media[1]
    return {
        "media": media,
        "rotate": rotate,
        "images": [(jpeg(image), width, height)],
        "content": f"q {w} 0 0 {h} {media[0]} {media[1]} cm /Im0 Do Q",
    }


def main():
    letter = (0, 0, 612, 792)

    content, words = layer(LINES, letter, (60, 700))
    save(
        "layer-rotated.pdf",
        pdf([{"media": letter, "rotate": 90, "content": content}]),
        [(792, 612, "layer", [[t, *turned(b, 90)] for t, *b in words])],
    )

    media, crop = (0, 0, 800, 1000), (100, 150, 712, 942)
    content, words = layer(LINES, crop, (160, 850))
    save(
        "layer-cropped.pdf",
        pdf([{"media": media, "crop": crop, "content": content}]),
        [(612, 792, "layer", words)],
    )

    image, words = drawn(1600, 1200)
    stored = image.transpose(Image.Transpose.ROTATE_90)
    exif = Image.Exif()
    exif[0x0112] = 6
    out = io.BytesIO()
    stored.convert("RGB").save(out, "JPEG", quality=92, exif=exif)
    save("photo-exif.jpg", out.getvalue(), [(1600, 1200, "ocr", words)])

    image, words = drawn(2200, 1700)
    page = image.transpose(Image.Transpose.ROTATE_90)
    save(
        "scan-sideways.pdf",
        pdf([scan_page(page, letter)]),
        [(612, 792, "ocr", [[t, *turned(b, 270)] for t, *b in words])],
    )

    image, words = drawn(1700, 2200)
    stored = image.transpose(Image.Transpose.ROTATE_90)
    save(
        "scan-rotated.pdf",
        pdf([scan_page(stored, (0, 0, 792, 612), rotate=90)]),
        [(612, 792, "ocr", words)],
    )

    image, words = drawn(1700, 2200)
    save(
        "scan-upside-down.pdf",
        pdf([scan_page(image.transpose(Image.Transpose.ROTATE_180), letter)]),
        [(612, 792, "ocr", [[t, *turned(b, 180)] for t, *b in words])],
    )

    image, words = drawn(1700, 2200)
    flipped = scan_page(image.transpose(Image.Transpose.ROTATE_180), letter)
    flipped["content"] = "q -612 0 0 -792 612 792 cm /Im0 Do Q"
    save("scan-drawn-turned.pdf", pdf([flipped]), [(612, 792, "ocr", words)])

    image, words = drawn(1700, 2200, top=0.25)
    page = scan_page(image, letter)
    stamp, stamped = layer(["KOPIE"], letter, (60, 730), size=24)
    page["content"] += "\n" + stamp
    save("scan-stamped.pdf", pdf([page]), [(612, 792, "ocr", stamped + words)])

    small, small_words = drawn(1275, 1650)
    large, large_words = drawn(2550, 3300)
    save(
        "scan-two-dpi.pdf",
        pdf([scan_page(small, letter), scan_page(large, letter)]),
        [(612, 792, "ocr", small_words), (612, 792, "ocr", large_words)],
    )


if __name__ == "__main__":
    main()
