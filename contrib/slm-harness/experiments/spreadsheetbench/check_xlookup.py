"""Probe actual native Calc calculation and XLSX round-trip, without cached fixtures."""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import time
import uuid
import zipfile
import xml.etree.ElementTree as ET

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'excel-smoke'))
from office import connect, props, load, save, stop_office


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default='/opt/libreoffice26.8/program/soffice')
    parser.add_argument('--output', default='artifacts/spreadsheetbench/xlookup-26.8')
    args = parser.parse_args()
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=True)
    pipe = 'xlookup_' + uuid.uuid4().hex
    command = [args.binary, f'-env:UserInstallation={(root / "profile").as_uri()}',
               '--headless', '--norestore', '--nodefault', '--nofirststartwizard',
               f'--accept=pipe,name={pipe};urp;StarOffice.ComponentContext']
    report = {'version': subprocess.check_output([args.binary, '--version'], text=True).strip(),
              'command': command, 'phases': {}}
    log = (root / 'office.log').open('wb')
    proc = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
    doc = None
    try:
        for _ in range(150):
            try:
                desktop = connect(pipe)
                break
            except Exception:
                if proc.poll() is not None:
                    raise RuntimeError('Office exited; see office.log')
                time.sleep(.1)
        else:
            raise TimeoutError('UNO startup')
        doc = desktop.loadComponentFromURL('private:factory/scalc', '_blank', 0, props(Hidden=True))
        source = doc.Sheets.getByIndex(0)
        source.Name = 'Source'
        source.getCellRangeByName('A1:C6').setDataArray((
            ('key', 'label', 'amount'), (1, 'one', 100), (2, 'two-first', 200),
            (2, 'two-last', 250), (4, 'four', 400), (5, 'five', 500)))
        source.getCellRangeByName('E1:F3').setDataArray(((10, 'ten'), (20, 'twenty'), (40, 'forty')))
        source.getCellRangeByName('H1:J2').setDataArray(((1, 2, 3), ('a', 'b', 'c')))
        doc.Sheets.insertNewByName('Checks', 1)
        sheet = doc.Sheets.getByName('Checks')
        cases = [
            ('exact_number', '=XLOOKUP(2;Source.A2:A6;Source.C2:C6)', 200),
            ('first_duplicate', '=XLOOKUP(2;Source.A2:A6;Source.B2:B6)', 'two-first'),
            ('missing_fallback', '=XLOOKUP(99;Source.A2:A6;Source.B2:B6;"missing")', 'missing'),
            ('missing_error', '=XLOOKUP(99;Source.A2:A6;Source.B2:B6)', '#N/A'),
            ('reverse', '=XLOOKUP(2;Source.A2:A6;Source.B2:B6;"missing";0;-1)', 'two-last'),
            ('left_lookup', '=XLOOKUP("two-first";Source.B2:B6;Source.A2:A6)', 2),
            ('next_smaller', '=XLOOKUP(25;Source.E1:E3;Source.F1:F3;"missing";-1)', 'twenty'),
            ('next_larger', '=XLOOKUP(25;Source.E1:E3;Source.F1:F3;"missing";1)', 'forty'),
            ('wildcard', '=XLOOKUP("tw*";Source.B2:B6;Source.A2:A6;"missing";2)', 2),
            ('horizontal', '=XLOOKUP(2;Source.H1:J1;Source.H2:J2)', 'b'),
        ]
        for row, (name, formula, _) in enumerate(cases):
            sheet.getCellByPosition(0, row).setString(name)
            sheet.getCellByPosition(1, row).setFormula(formula)
        sheet.getCellRangeByName('B12:C12').setArrayFormula('=XLOOKUP(2;Source.A2:A6;Source.B2:C6)')

        def check(phase, amount=200):
            doc.calculateAll()
            checks = doc.Sheets.getByName('Checks')
            results = []
            for row, (name, _, expected) in enumerate(cases):
                if name == 'exact_number':
                    expected = amount
                cell = checks.getCellByPosition(1, row)
                value = cell.getString() if isinstance(expected, str) else cell.getValue()
                passed = value == expected and (cell.getError() != 0 if name == 'missing_error' else cell.getError() == 0)
                results.append(dict(name=name, expected=expected, actual=value, error=cell.getError(), formula=cell.getFormula(), passed=passed))
            actual = list(checks.getCellRangeByName('B12:C12').getDataArray()[0])
            results.append(dict(name='array_result', expected=['two-first', amount], actual=actual, passed=actual == ['two-first', amount]))
            report['phases'][phase] = results

        check('native')
        path = root / 'xlookup.xlsx'
        save(doc, path)
        doc.close(True)
        doc = load(desktop, path)
        check('xlsx_reopen')
        doc.Sheets.getByName('Source').getCellRangeByName('C3').setValue(777)
        check('dependency_recalculation', 777)
        with zipfile.ZipFile(path) as archive:
            ns = {'s': 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'}
            xml = ET.fromstring(archive.read('xl/worksheets/sheet2.xml'))
            report['exported_formulas'] = [node.text for node in xml.findall('.//s:f', ns)]
        report['passed'] = all(r['passed'] for phase in report['phases'].values() for r in phase)
        report['passed'] = report['passed'] and all('XLOOKUP' in f for f in report['exported_formulas']) and len(report['exported_formulas']) == 11
    except Exception as exc:
        report.update(passed=False, exception=repr(exc))
        raise
    finally:
        if doc is not None:
            doc.close(True)
        stop_office(proc, log)
        (root / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({'version': report['version'], 'passed': report['passed'], 'report': str(root / 'report.json')}, ensure_ascii=False))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
