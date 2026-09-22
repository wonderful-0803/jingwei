"""Independent document grader. It never writes to the model output."""
from pathlib import Path

from docx import Document

from normalize import normalize, text_content


def _heading_levels(doc):
    return {p['text']: int(p['style'].split()[-1])
            for p in doc['paragraphs']
            if p['style'].startswith('Heading ') and p['style'].split()[-1].isdigit()}


def _contains_text(doc, value):
    return value in text_content(doc)


def grade(spec, input_path, gold_path, output_path, render=None):
    result = {
        'id': spec['id'], 'artifact_valid': False, 'structure_passed': False,
        'content_passed': False, 'render_passed': None, 'delivery_completed': False,
        'failures': [],
    }
    try:
        Document(output_path)
        actual = normalize(output_path)
        result['artifact_valid'] = True
    except Exception as exc:
        result['failures'].append('artifact_invalid:' + type(exc).__name__)
        return result
    expected = spec.get('expected', {})
    actual_text = text_content(actual)
    input_norm = normalize(input_path)
    structural_failures = []
    content_failures = []
    for value in spec.get('protected_text', []):
        if value not in actual_text:
            content_failures.append('protected_text_missing')
    for value in expected.get('contains', []):
        if value not in actual_text:
            content_failures.append('contains_missing:' + value)
    for value in expected.get('excludes', []):
        if value in actual_text:
            content_failures.append('excludes_present:' + value)
    if 'heading_levels' in expected and _heading_levels(actual) != expected['heading_levels']:
        structural_failures.append('heading_levels_mismatch')
    if 'table_rows' in expected:
        rows = actual['tables'][0]['rows'] if actual['tables'] else []
        if rows != expected['table_rows']: structural_failures.append('table_rows_mismatch')
    if 'all_body_style' in expected:
        if any(p['style'] != expected['all_body_style'] for p in actual['paragraphs'][1:]):
            structural_failures.append('body_style_mismatch')
    if 'title' in expected and (not actual['paragraphs'] or actual['paragraphs'][0]['text'] != expected['title']):
        content_failures.append('title_mismatch')
    if 'table_cell' in expected:
        if not any(expected['table_cell'][0] in row and expected['table_cell'][1] in row
                   for table in actual['tables'] for row in table['rows']):
            content_failures.append('table_cell_missing')
    if 'table_first_column' in expected:
        column = [row[0] for row in actual['tables'][0]['rows']] if actual['tables'] else []
        if column != expected['table_first_column']: structural_failures.append('table_first_column_mismatch')
    if 'table_rows' in expected:
        rows = actual['tables'][0]['rows'] if actual['tables'] else []
        if rows != expected['table_rows']: structural_failures.append('table_rows_mismatch')
    if expected.get('table_count') is not None and len(actual['tables']) != expected['table_count']:
        structural_failures.append('table_count_mismatch')
    if 'numbered_items' in expected:
        found = [p['text'] for p in actual['paragraphs'] if p['style'] == 'List Number']
        if found != expected['numbered_items']: structural_failures.append('numbered_items_mismatch')
    if 'bullet_items' in expected:
        found = [p['text'] for p in actual['paragraphs'] if p['style'] == 'List Bullet']
        if found != expected['bullet_items']: structural_failures.append('bullet_items_mismatch')
    if 'paragraph_order' in expected:
        order = [p['text'] for p in actual['paragraphs'] if p['text'] in expected['paragraph_order']]
        if order != expected['paragraph_order']: structural_failures.append('paragraph_order_mismatch')
    if expected.get('caption_before_table'):
        caption = next((i for i,p in enumerate(actual['paragraphs']) if p['text'] == '表 1 项目清单'), None)
        if caption is None or not actual['tables']: structural_failures.append('caption_missing')
    if expected.get('sop_headings') and _heading_levels(actual).get('目的') != 1:
        structural_failures.append('sop_heading_missing')
    if 'page_break_before' in expected:
        matched = next((p for p in actual['paragraphs'] if p['text'] == expected['page_break_before']), None)
        if not matched or not matched.get('page_break_before'):
            structural_failures.append('page_break_before_missing')
    if expected.get('changed') and normalize(output_path) == input_norm:
        structural_failures.append('output_unchanged')
    if 'orientation' in expected and actual['sections'][0]['orientation'] != expected['orientation']:
        structural_failures.append('orientation_mismatch')
    if 'margin_inches' in expected:
        target = int(expected['margin_inches'] * 1440)
        if any(abs(v - target) > 60 for v in actual['sections'][0]['margins'].values()):
            structural_failures.append('margin_mismatch')
    if 'header' in expected and expected['header'] not in text_content({'paragraphs': [p for group in actual['headers'] for p in group], 'tables': []}):
        structural_failures.append('header_mismatch')
    if 'footer' in expected and expected['footer'] not in text_content({'paragraphs': [p for group in actual['footers'] for p in group], 'tables': []}):
        structural_failures.append('footer_mismatch')
    if 'media_count' in expected and actual['media_count'] != expected['media_count']:
        structural_failures.append('media_count_mismatch')
    if actual['media_count'] < input_norm['media_count']:
        structural_failures.append('protected_media_removed')
    result['failures'] = structural_failures + content_failures
    result['structure_passed'] = not structural_failures
    result['content_passed'] = not content_failures
    result['delivery_completed'] = result['artifact_valid']
    if render is not None:
        result['render_passed'] = bool(render.get('ok'))
        if not result['render_passed']: result['failures'].append('render_failed')
    return result
