#!/usr/bin/env python3
"""Creates the documents for the text extraction tests (needs pdftoppm and ImageMagick):

- rechnung.pdf: a PDF with a text layer
- scan.pdf:     the image of a page, without text layer (as a scanner makes it)
- scan.tif:     a scanned page as TIFF

Run from this directory: ./make.py
"""

import subprocess
import tempfile
import zlib
from pathlib import Path


def pdf(objects, path):
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for i, obj in enumerate(objects, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + obj + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
    for o in offsets:
        out += b"%010d 00000 n \n" % o
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (
        len(objects) + 1,
        xref,
    )
    Path(path).write_bytes(bytes(out))


def stream(data, extra=b""):
    return b"<< /Length %d %s>>\nstream\n" % (len(data), extra) + data + b"\nendstream"


def text_pdf(lines, path, w=420, h=200, size=24):
    body = "BT /F1 %d Tf 24 %d Td %d TL\n" % (size, h - 48, size + 10)
    for line in lines:
        body += "(%s) Tj T*\n" % line.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
    body += "ET"
    pdf(
        [
            b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 %d %d] "
            b"/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>" % (w, h),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
            stream(body.encode("cp1252")),
        ],
        path,
    )


def page_image(lines, png):
    """Renders lines of text as a grey page image (200 dpi)."""
    with tempfile.TemporaryDirectory() as d:
        src = Path(d) / "seite.pdf"
        text_pdf(lines, src)
        subprocess.run(
            ["pdftoppm", "-r", "200", "-gray", "-png", "-singlefile", str(src), str(Path(png).with_suffix(""))],
            check=True,
        )


def image_pdf(png, path, w=420, h=200):
    size = subprocess.run(["identify", "-format", "%w %h", png], check=True, capture_output=True).stdout
    pw, ph = map(int, size.split())
    gray = subprocess.run(["convert", png, "-depth", "8", "gray:-"], check=True, capture_output=True).stdout
    image = stream(
        zlib.compress(gray, 9),
        b"/Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceGray "
        b"/BitsPerComponent 8 /Filter /FlateDecode " % (pw, ph),
    )
    pdf(
        [
            b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 %d %d] "
            b"/Resources << /XObject << /Im1 4 0 R >> >> /Contents 5 0 R >>" % (w, h),
            image,
            stream(b"q %d 0 0 %d 0 0 cm /Im1 Do Q" % (w, h)),
        ],
        path,
    )


if __name__ == "__main__":
    text_pdf(["Rechnung 4711", "Heizungswartung bei Familie Müller", "Wärmepumpe geprüft"], "rechnung.pdf")
    with tempfile.TemporaryDirectory() as d:
        png = str(Path(d) / "a.png")
        page_image(["Mietvertrag", "Wohnung in der Gartenstrasse", "Kaution drei Monatsmieten"], png)
        image_pdf(png, "scan.pdf")
        png = str(Path(d) / "b.png")
        page_image(["Kontoauszug Sparkasse", "Dezember Abschluss"], png)
        subprocess.run(["convert", png, "-compress", "lzw", "scan.tif"], check=True)
