"""Word plugin contract: constrained operations over an isolated DOCX copy.

The model never receives the gold document and never writes files directly. It
returns JSON operations; this module validates and executes those operations in
the task sandbox before the independent grader sees the result.
"""
from copy import deepcopy
import re
from pathlib import Path

from docx import Document
from docx.enum.section import WD_ORIENT
from docx.shared import Inches


OPERATION_SCHEMA = {
    "type": "object",
    "properties": {
        "operations": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "op": {"type": "string"}, "text": {"type": "string"},
                    "old": {"type": "string"}, "new": {"type": "string"},
                    "style": {"type": "string"}, "title": {"type": "string"},
                    "rows": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}},
                    "table_index": {"type": "integer"}, "row_index": {"type": "integer"},
                    "col_index": {"type": "integer"}, "row_key": {"type": "string"},
                    "value": {"type": "string"}, "descending": {"type": "boolean"},
                    "orientation": {"type": "string"}, "margin_inches": {"type": "number"},
                    "header": {"type": "string"}, "footer": {"type": "string"},
                    "match": {"type": "string"}, "replacement": {"type": "string"},
                    "texts": {"type": "array", "items": {"type": "string"}},
                    "remove_text": {"type": "string"}
                },
                "required": ["op"], "additionalProperties": False
            }
        }
    },
    "required": ["operations"], "additionalProperties": False
}

ALLOWED_OPS = {
    "replace_text", "append_paragraph", "set_style", "set_title", "add_heading",
    "create_table", "set_cell", "sort_table", "table_to_text", "caption_table",
    "set_page", "page_break_before", "header_footer", "remove_text", "numbered_list",
}


def snapshot(path):
    doc = Document(path)
    return {
        "paragraphs": [{"text": p.text, "style": p.style.name} for p in doc.paragraphs],
        "tables": [[[cell.text for cell in row.cells] for row in table.rows] for table in doc.tables],
        "sections": [{
            "orientation": "landscape" if section.orientation == WD_ORIENT.LANDSCAPE else "portrait",
            "margins_inches": {
                "top": round(section.top_margin.inches, 3), "bottom": round(section.bottom_margin.inches, 3),
                "left": round(section.left_margin.inches, 3), "right": round(section.right_margin.inches, 3),
            },
        } for section in doc.sections],
    }


def _replace_runs(paragraph, old, new):
    changed = False
    for run in paragraph.runs:
        if old in run.text:
            run.text = run.text.replace(old, new)
            changed = True
    if not changed and old in paragraph.text:
        paragraph.text = paragraph.text.replace(old, new)
        changed = True
    return changed


def _table(doc, op):
    index = int(op.get("table_index", 0))
    if index < 0 or index >= len(doc.tables):
        raise ValueError(f"table_index out of range: {index}")
    return doc.tables[index]


def _remove_paragraph(paragraph):
    paragraph._element.getparent().remove(paragraph._element)


def _normalize_payload(payload):
    """Accept harmless field aliases emitted by local models.

    The schema is intentionally small, but Qwen variants sometimes put a
    style in ``title``/``value`` or a replacement in ``replacement``. Mapping
    those aliases here keeps protocol tolerance separate from grading logic.
    """
    result = deepcopy(payload)
    operations = result["operations"]
    styles = {"Normal", "Title", "Caption", "Quote", "Heading 1", "Heading 2", "Heading 3", "List Bullet", "List Number"}
    for op in operations:
        name = op.get("op")
        if name in {"append_paragraph", "add_heading"}:
            if not op.get("style"):
                for alias in ("title", "value"):
                    if op.get(alias) in styles: op["style"] = op[alias]; break
        elif name == "set_style":
            if op.get("value") in styles and not op.get("style"): op["style"] = op["value"]
            if op.get("title") in styles and not op.get("style"): op["style"] = op["title"]
            if not op.get("match"): op["match"] = op.get("text", "")
        elif name == "set_title":
            if op.get("text") and op.get("title") in styles:
                op["op"], op["match"], op["style"] = "set_style", op["text"], op["title"]
            elif not op.get("title") and op.get("text"): op["title"] = op["text"]
        elif name == "replace_text":
            if not op.get("new") and op.get("replacement"): op["new"] = op["replacement"]
            if not op.get("new") and op.get("text"): op["new"] = op["text"]
            if not op.get("old") and op.get("texts"):
                op["old"] = op["texts"][0]
                if not op.get("new") and op.get("text"): op["new"] = op["text"]
        elif name == "remove_text" and not op.get("match"):
            op["match"] = op.get("text", "")
    # Models often emit an empty create_table followed by one set_cell per
    # cell. Recover the coordinates from either {row,col} or sequence order.
    for index, op in enumerate(operations):
        if op.get("op") != "create_table" or op.get("rows"): continue
        cells = []
        for candidate in operations[index + 1:]:
            if candidate.get("op") != "set_cell": break
            value = str(candidate.get("value", "")); text = str(candidate.get("text", candidate.get("replacement", "")))
            match = re.fullmatch(r"\{\s*(\d+)\s*,\s*(\d+)\s*\}", value)
            if match: row, col, cell_value = int(match.group(1)), int(match.group(2)), text
            else:
                row, col = divmod(len(cells), 3); cell_value = value if value else text
            cells.append((row, col, cell_value))
        if cells:
            rows = [["" for _ in range(max(col for _, col, _ in cells) + 1)] for _ in range(max(row for row, _, _ in cells) + 1)]
            for row, col, value in cells: rows[row][col] = value
            op["rows"] = rows
    return result


def apply_operations(input_path, output_path, payload):
    """Apply a model payload to a fresh copy and save output_path."""
    if not isinstance(payload, dict) or not isinstance(payload.get("operations"), list):
        raise ValueError("payload must contain an operations array")
    payload = _normalize_payload(payload)
    doc = Document(input_path)
    applied = []
    for raw in payload["operations"]:
        if not isinstance(raw, dict) or raw.get("op") not in ALLOWED_OPS:
            raise ValueError(f"unsupported operation: {raw!r}")
        op = raw["op"]
        if op == "replace_text":
            old, new = str(raw.get("old", "")), str(raw.get("new", ""))
            if not old: raise ValueError("replace_text requires old")
            for paragraph in doc.paragraphs: _replace_runs(paragraph, old, new)
            for table in doc.tables:
                for row in table.rows:
                    for cell in row.cells:
                        for paragraph in cell.paragraphs: _replace_runs(paragraph, old, new)
        elif op in {"append_paragraph", "add_heading"}:
            style = raw.get("style", "Heading 1" if op == "add_heading" else "Normal")
            doc.add_paragraph(str(raw.get("text", raw.get("title", ""))), style=style)
        elif op == "set_style":
            match, style = str(raw.get("match", "")), str(raw.get("style", "Normal"))
            for p in doc.paragraphs:
                if p.text == match or p.style.name == match: p.style = style
        elif op == "set_title":
            if not doc.paragraphs: doc.add_paragraph()
            doc.paragraphs[0].text = str(raw.get("title", raw.get("text", "")))
            doc.paragraphs[0].style = raw.get("style", "Title")
        elif op == "create_table":
            rows = raw.get("rows") or []
            if not rows: raise ValueError("create_table requires rows")
            for p in list(doc.paragraphs):
                if p.text and ("|" in p.text or p.text == raw.get("remove_text")): _remove_paragraph(p)
            table = doc.add_table(rows=0, cols=max(len(row) for row in rows)); table.style = "Table Grid"
            for values in rows:
                cells = table.add_row().cells
                for cell, value in zip(cells, values): cell.text = str(value)
        elif op == "set_cell":
            table = _table(doc, raw); row_index, col_index = raw.get("row_index"), raw.get("col_index")
            if row_index is None and raw.get("row_key") is not None:
                row_index = next((i for i, row in enumerate(table.rows) if raw["row_key"] in row.cells[0].text), None)
            if row_index is None: raise ValueError("set_cell requires row_index or row_key")
            table.cell(int(row_index), int(col_index or 0)).text = str(raw.get("value", ""))
        elif op == "sort_table":
            table = _table(doc, raw); header = [[c.text for c in table.rows[0].cells]]
            values = [[c.text for c in row.cells] for row in table.rows[1:]]
            col = int(raw.get("col_index", 1)); descending = bool(raw.get("descending", True))
            values.sort(key=lambda r: float(r[col]) if str(r[col]).replace('.', '', 1).isdigit() else str(r[col]), reverse=descending)
            for row, vals in zip(table.rows, header + values):
                for cell, value in zip(row.cells, vals): cell.text = value
        elif op == "table_to_text":
            table = _table(doc, raw)
            values = [[c.text for c in row.cells] for row in table.rows]
            anchor = table._element
            parent = anchor.getparent(); index = parent.index(anchor)
            parent.remove(anchor)
            for key, value in values[1:]:
                p = doc.add_paragraph(f"{key}：{value}", style="List Bullet")
                parent.insert(index, p._element); index += 1
        elif op == "caption_table":
            doc.add_paragraph(str(raw.get("text", raw.get("title", "表 1 项目清单"))), style="Caption")
        elif op == "set_page":
            section = doc.sections[0]; orientation = raw.get("orientation", "portrait")
            if orientation == "landscape":
                section.orientation = WD_ORIENT.LANDSCAPE
                section.page_width, section.page_height = section.page_height, section.page_width
            if raw.get("margin_inches") is not None:
                margin = Inches(float(raw["margin_inches"]))
                for attr in ("top_margin", "bottom_margin", "left_margin", "right_margin"): setattr(section, attr, margin)
        elif op == "page_break_before":
            match = str(raw.get("match", raw.get("text", "")))
            for p in doc.paragraphs:
                if p.text == match: p.paragraph_format.page_break_before = True
        elif op == "header_footer":
            section = doc.sections[0]
            if raw.get("header") is not None: section.header.paragraphs[0].text = str(raw["header"])
            if raw.get("footer") is not None: section.footer.paragraphs[0].text = str(raw["footer"])
        elif op == "remove_text":
            match = str(raw.get("match", "")); replacement = str(raw.get("replacement", ""))
            for p in doc.paragraphs:
                if match in p.text: _replace_runs(p, match, replacement)
        elif op == "numbered_list":
            for text in raw.get("texts", [raw.get("text", "")]):
                for p in doc.paragraphs:
                    if p.text == text: p.style = "List Number"
        applied.append(op)
    Path(output_path).parent.mkdir(parents=True, exist_ok=True)
    doc.save(output_path)
    return {"applied": applied, "operation_count": len(applied), "output": str(output_path)}
