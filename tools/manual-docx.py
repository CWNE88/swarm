#!/usr/bin/env python3
"""Makes the Word copy of Swarm's manual from the one the program has
built in (app/manual/manual.md, whose format is in a note at its top):
a title page, the contents with page numbers, chapters and sections
numbered by Word's own multilevel numbering (Heading 1-3, so they
renumber themselves if the document's edited), tables, and the pictures.

Usage: tools/manual-docx.py [out.docx]      (default target/swarm-manual.docx)

Needs python-docx, LibreOffice (soffice: the pictures, and a first render
for the contents' page numbers) and poppler (pdftotext, pdfinfo):
    python3 -m venv /tmp/docx && /tmp/docx/bin/pip install python-docx
    /tmp/docx/bin/python tools/manual-docx.py
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import date

try:
    from docx import Document
    from docx.enum.section import WD_SECTION
    from docx.enum.table import WD_TABLE_ALIGNMENT
    from docx.enum.text import WD_ALIGN_PARAGRAPH, WD_BREAK, WD_TAB_ALIGNMENT, WD_TAB_LEADER
    from docx.oxml import OxmlElement
    from docx.oxml.ns import qn
    from docx.shared import Cm, Pt, RGBColor
except ImportError:
    sys.exit("needs python-docx: python3 -m venv /tmp/docx && /tmp/docx/bin/pip install python-docx, then run it with /tmp/docx/bin/python")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FOLDER = os.path.join(ROOT, "app", "manual")
OUT = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else os.path.join(ROOT, "target", "swarm-manual.docx")

ACCENT = RGBColor(0x1F, 0x4E, 0x79)
DIM = RGBColor(0x59, 0x59, 0x59)
MONO = "Consolas"


# ---- Reading the source, as app/src/manual.rs does -----------------------

def read():
    """The sections: (number, title, level, blocks), each block a tuple
    whose first item says what it is."""
    text = open(os.path.join(FOLDER, "manual.md"), encoding="utf-8").read()
    text = re.sub(r"<!--.*?-->", "", text, flags=re.S)
    lines = text.split("\n")
    sections, counts = [], [0, 0, 0]
    i = 0
    while i < len(lines):
        line = lines[i].rstrip()
        i += 1
        if not line.strip():
            continue
        m = re.match(r"^(#{1,3}) (.+)$", line)
        if m:
            level = len(m.group(1))
            counts[level - 1] += 1
            for j in range(level, 3):
                counts[j] = 0
            sections.append([".".join(str(c) for c in counts[:level]), m.group(2).strip(), level, []])
            continue
        blocks = sections[-1][3]
        if line.startswith("#### "):
            blocks.append(("heading", line[5:].strip()))
        elif line.startswith("  - "):
            blocks.append(("under", line[4:].strip()))
        elif line.startswith("- "):
            blocks.append(("bullet", line[2:].strip()))
        elif re.match(r"^\d+\. ", line):
            n, rest = line.split(". ", 1)
            blocks.append(("step", n, rest.strip()))
        elif line.startswith("!["):
            caption, file = re.match(r"^!\[(.*)\]\((.*)\)$", line).groups()
            blocks.append(("picture", file, caption))
        elif line.startswith("|"):
            rows = [cells(line)]
            while i < len(lines) and lines[i].startswith("|"):
                row = cells(lines[i])
                if not row[0].startswith("---"):
                    rows.append(row)
                i += 1
            blocks.append(("table", rows[0], rows[1:]))
        elif len(line) > 2 and line[0] == "`" and line[-1] == "`" and "`" not in line[1:-1]:
            blocks.append(("command", line[1:-1]))
        else:
            blocks.append(("para", line.strip()))
    return sections


def cells(line):
    out, cell, code = [], "", False
    for c in line.strip().lstrip("|"):
        if c == "`":
            code = not code
            cell += c
        elif c == "|" and not code:
            out.append(cell.strip())
            cell = ""
        else:
            cell += c
    if cell.strip():
        out.append(cell.strip())
    return out


SECTIONS = read()
NUMBERS = {title: number for number, title, _, _ in SECTIONS}


def runs(paragraph, text, mono=False, bold=False):
    """Text with **bold**, `code` and [[links]] (as their numbers)."""
    text = re.sub(r"\[\[(.+?)\]\]", lambda m: NUMBERS.get(m.group(1), m.group(1)), text)
    for piece in re.split(r"(\*\*.+?\*\*|`[^`]+`)", text):
        if not piece:
            continue
        if piece.startswith("**") and piece.endswith("**") and len(piece) > 4:
            run = paragraph.add_run(piece[2:-2])
            run.bold = True
        elif piece.startswith("`") and piece.endswith("`") and len(piece) > 2:
            run = paragraph.add_run(piece[1:-1])
            run.font.name = MONO
            run.font.size = Pt(9.5)
        else:
            run = paragraph.add_run(piece)
            if mono:
                run.font.name = MONO
                run.font.size = Pt(9.5)
        if bold:
            run.bold = True


# ---- The document ---------------------------------------------------------

def field(paragraph, kind, instr=None):
    r = OxmlElement("w:r")
    c = OxmlElement("w:fldChar")
    c.set(qn("w:fldCharType"), kind)
    r.append(c)
    paragraph._p.append(r)
    if instr:
        r = OxmlElement("w:r")
        t = OxmlElement("w:instrText")
        t.set(qn("xml:space"), "preserve")
        t.text = f" {instr} "
        r.append(t)
        paragraph._p.append(r)


def heading_numbering(doc):
    numbering = doc.part.numbering_part.element
    abstract = OxmlElement("w:abstractNum")
    abstract.set(qn("w:abstractNumId"), "90")
    multi = OxmlElement("w:multiLevelType")
    multi.set(qn("w:val"), "multilevel")
    abstract.append(multi)
    for level in range(3):
        lvl = OxmlElement("w:lvl")
        lvl.set(qn("w:ilvl"), str(level))
        for tag, val in (("w:start", "1"), ("w:numFmt", "decimal"), ("w:pStyle", f"Heading{level + 1}"),
                         ("w:lvlText", ".".join(f"%{i + 1}" for i in range(level + 1))), ("w:lvlJc", "left")):
            e = OxmlElement(tag)
            e.set(qn("w:val"), val)
            lvl.append(e)
        ppr = OxmlElement("w:pPr")
        ind = OxmlElement("w:ind")
        hang = [737, 907, 1077][level]
        ind.set(qn("w:left"), str(hang))
        ind.set(qn("w:hanging"), str(hang))
        ppr.append(ind)
        lvl.append(ppr)
        abstract.append(lvl)
    first = numbering.find(qn("w:num"))
    if first is not None:
        first.addprevious(abstract)
    else:
        numbering.append(abstract)
    num = OxmlElement("w:num")
    num.set(qn("w:numId"), "90")
    a = OxmlElement("w:abstractNumId")
    a.set(qn("w:val"), "90")
    num.append(a)
    numbering.append(num)
    for level in range(3):
        ppr = doc.styles[f"Heading {level + 1}"].element.get_or_add_pPr()
        num_pr = OxmlElement("w:numPr")
        ilvl = OxmlElement("w:ilvl")
        ilvl.set(qn("w:val"), str(level))
        num_id = OxmlElement("w:numId")
        num_id.set(qn("w:val"), "90")
        num_pr.append(ilvl)
        num_pr.append(num_id)
        ppr.insert(0, num_pr)


def fonts(element, name):
    rpr = element.get_or_add_rPr()
    f = rpr.find(qn("w:rFonts"))
    if f is None:
        f = OxmlElement("w:rFonts")
        rpr.insert(0, f)
    for attr in ("w:ascii", "w:hAnsi", "w:cs", "w:eastAsia"):
        f.set(qn(attr), name)
    for theme in ("w:asciiTheme", "w:hAnsiTheme", "w:eastAsiaTheme", "w:cstheme"):
        if f.get(qn(theme)) is not None:
            del f.attrib[qn(theme)]


def styles(doc):
    normal = doc.styles["Normal"]
    normal.font.name = "Calibri"
    normal.font.size = Pt(11)
    normal.paragraph_format.space_after = Pt(6)
    normal.paragraph_format.line_spacing = 1.15
    fonts(normal.element, "Calibri")
    for level, size, before, after in ((1, 20, 0, 12), (2, 14, 18, 6), (3, 12, 12, 4), (4, 11, 10, 3)):
        h = doc.styles[f"Heading {level}"]
        h.font.size = Pt(size)
        h.font.bold = True
        h.font.italic = False
        h.font.color.rgb = ACCENT if level < 4 else DIM
        h.paragraph_format.space_before = Pt(before)
        h.paragraph_format.space_after = Pt(after)
        h.paragraph_format.keep_with_next = True
        fonts(h.element, "Calibri")
    doc.styles["Heading 1"].paragraph_format.page_break_before = True
    for name in ("List Bullet", "List Bullet 2"):
        doc.styles[name].paragraph_format.space_after = Pt(3)


def shade(cell, fill):
    shd = OxmlElement("w:shd")
    shd.set(qn("w:val"), "clear")
    shd.set(qn("w:color"), "auto")
    shd.set(qn("w:fill"), fill)
    cell._tc.get_or_add_tcPr().append(shd)


def table(doc, head, rows):
    wide = any(r[0].startswith("`swarm") for r in rows)
    widths = (Cm(7.5), Cm(8.5)) if wide else (Cm(4.5), Cm(11.5))
    t = doc.add_table(rows=1, cols=2)
    t.style = "Table Grid"
    t.alignment = WD_TABLE_ALIGNMENT.LEFT
    t.autofit = False
    for col, width in zip(t._tbl.tblGrid.findall(qn("w:gridCol")), widths):
        col.set(qn("w:w"), str(int(width.twips)))
    for cell, text, width in zip(t.rows[0].cells, head, widths):
        cell.width = width
        runs(cell.paragraphs[0], text, bold=True)
        shade(cell, "D9E2F3")
    for key, value in rows:
        cells_ = t.add_row().cells
        for cell, width in zip(cells_, widths):
            cell.width = width
        runs(cells_[0].paragraphs[0], key, bold=not key.startswith("`"))
        runs(cells_[1].paragraphs[0], value)
    for row in t.rows:
        for cell in row.cells:
            cell.paragraphs[0].paragraph_format.space_after = Pt(2)
    doc.add_paragraph().paragraph_format.space_after = Pt(2)


def png_of(svg, work):
    """A picture as PNG, drawn by LibreOffice (ImageMagick's own SVG
    drawing gets the arrows wrong)."""
    shutil.copy(os.path.join(FOLDER, svg), work)
    subprocess.run(["soffice", "--headless", "--convert-to", "png", "--outdir", work, os.path.join(work, svg)], check=True, capture_output=True)
    return os.path.join(work, os.path.splitext(svg)[0] + ".png")


def build(out, pages, work):
    doc = Document()
    section = doc.sections[0]
    section.page_width, section.page_height = Cm(21), Cm(29.7)
    section.left_margin = section.right_margin = Cm(2.5)
    section.top_margin = section.bottom_margin = Cm(2.2)
    styles(doc)
    heading_numbering(doc)

    # The title page.
    for _ in range(8):
        doc.add_paragraph()
    for text, size, colour in (("Swarm", 40, ACCENT), ("User manual", 20, DIM)):
        r = doc.add_paragraph().add_run(text)
        r.font.size = Pt(size)
        r.font.color.rgb = colour
        r.bold = size == 40
    p = doc.add_paragraph()
    p.paragraph_format.space_before = Pt(24)
    p.add_run("Serverless, end-to-end encrypted messages, calls, video and files").font.size = Pt(12)
    r = doc.add_paragraph().add_run(date.today().strftime("%-d %B %Y"))
    r.font.color.rgb = DIM
    doc.add_paragraph().add_run().add_break(WD_BREAK.PAGE)

    # The contents: a Word field, holding what a first render found so it
    # reads right before Word updates it.
    r = doc.add_paragraph().add_run("Contents")
    r.bold = True
    r.font.size = Pt(20)
    r.font.color.rgb = ACCENT
    entries = [(n, t, l) for n, t, l, _ in SECTIONS]
    first = doc.add_paragraph()
    field(first, "begin", 'TOC \\o "1-3" \\h \\z \\u')
    field(first, "separate")
    for k, (number, title, level) in enumerate(entries):
        q = first if k == 0 else doc.add_paragraph()
        q.paragraph_format.left_indent = Cm(0.6 * (level - 1))
        q.paragraph_format.space_after = Pt(4 if level == 1 else 2)
        q.paragraph_format.space_before = Pt(6 if level == 1 else 0)
        q.paragraph_format.tab_stops.add_tab_stop(Cm(16), WD_TAB_ALIGNMENT.RIGHT, WD_TAB_LEADER.DOTS)
        r = q.add_run(f"{number}  {title}\t{pages.get(number, '')}")
        r.bold = level == 1
    field(doc.add_paragraph(), "end")

    body = doc.add_section(WD_SECTION.NEW_PAGE)
    body.footer.is_linked_to_previous = False
    foot = body.footer.paragraphs[0]
    foot.alignment = WD_ALIGN_PARAGRAPH.CENTER
    field(foot, "begin", "PAGE")
    field(foot, "separate")
    foot.add_run("1")
    field(foot, "end")

    for number, title, level, blocks in SECTIONS:
        doc.add_heading(title, level=level)
        for b in blocks:
            kind = b[0]
            if kind == "heading":
                doc.add_heading(b[1], level=4)
            elif kind == "para":
                runs(doc.add_paragraph(), b[1])
            elif kind in ("bullet", "under"):
                runs(doc.add_paragraph(style="List Bullet" if kind == "bullet" else "List Bullet 2"), b[1])
            elif kind == "step":
                p = doc.add_paragraph()
                p.paragraph_format.left_indent = Cm(0.9)
                p.paragraph_format.first_line_indent = Cm(-0.6)
                p.paragraph_format.space_after = Pt(3)
                p.add_run(f"{b[1]}.\t")
                runs(p, b[2])
            elif kind == "command":
                p = doc.add_paragraph()
                p.paragraph_format.left_indent = Cm(1)
                runs(p, b[1], mono=True)
            elif kind == "table":
                table(doc, b[1], b[2])
            elif kind == "picture":
                doc.add_picture(png_of(b[1], work), width=Cm(16))
                doc.paragraphs[-1].alignment = WD_ALIGN_PARAGRAPH.CENTER
                cap = doc.add_paragraph()
                cap.alignment = WD_ALIGN_PARAGRAPH.CENTER
                r = cap.add_run(b[2])
                r.italic = True
                r.font.color.rgb = DIM

    upd = OxmlElement("w:updateFields")
    upd.set(qn("w:val"), "true")
    doc.settings.element.append(upd)
    doc.core_properties.title = "Swarm user manual"
    doc.save(out)


def page_numbers(docx, work):
    """Where each heading landed, from a render of `docx`."""
    subprocess.run(["soffice", "--headless", "--convert-to", "pdf", "--outdir", work, docx], check=True, capture_output=True)
    pdf = os.path.join(work, os.path.splitext(os.path.basename(docx))[0] + ".pdf")
    count = int(re.search(r"Pages:\s+(\d+)", subprocess.run(["pdfinfo", pdf], capture_output=True, text=True).stdout).group(1))
    texts = [subprocess.run(["pdftotext", "-layout", "-f", str(n), "-l", str(n), pdf, "-"], capture_output=True, text=True).stdout for n in range(1, count + 1)]
    # The body starts where chapter 1 does, after the contents.
    at = next(n for n, t in enumerate(texts) if n > 1 and re.search(r"^\s*1\s+" + re.escape(SECTIONS[0][1]) + r"\s*$", t, re.M))
    found = {}
    for number, title, _, _ in SECTIONS:
        pattern = re.compile(r"^\s*" + re.escape(number) + r"\s*" + re.escape(title[:30]), re.M)
        for n in range(at, count):
            if pattern.search(texts[n]):
                found[number] = str(n + 1)
                at = n
                break
    return found


def main():
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with tempfile.TemporaryDirectory() as work:
        first = os.path.join(work, "first.docx")
        build(first, {}, work)
        pages = page_numbers(first, work)
        build(OUT, pages, work)
        # Filling the contents can move nothing, as it's as long either way.
        if page_numbers(OUT, work) != pages:
            print("warning: the contents' page numbers may be off; Word puts them right when it updates the field")
    print(f"{OUT}: {len(SECTIONS)} sections, {max(int(p) for p in pages.values())} pages")


main()
