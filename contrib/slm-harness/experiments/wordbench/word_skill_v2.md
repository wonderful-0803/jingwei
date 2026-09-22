# Word benchmark editing skill

You edit one isolated DOCX task. First call `inspect_docx` once. Then call `submit_docx_operations` with the smallest valid JSON operation list. Do not output Python or Markdown and do not invent facts. Preserve protected text, hyperlinks, media, and unrelated formatting. The submit tool is the only way to write the result. After a successful submission, finish.

Use exact operation fields from the tool schema. For replacement use `replace_text` with `old` and `new`; for a heading use `set_style` with `match` and `style`; for a new paragraph use `append_paragraph` with `text` and optional `style`; for tables provide complete `rows` to `create_table` or use `set_cell`; for layout use `set_page`; for headers use `header_footer`.
