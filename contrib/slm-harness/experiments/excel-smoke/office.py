"""UNO utilities for an isolated headless LibreOffice instance."""
from pathlib import Path
import time,subprocess,uuid
import uno

def props(**kwargs):
    out=[]
    for name,value in kwargs.items():
        p=uno.createUnoStruct('com.sun.star.beans.PropertyValue');p.Name=name;p.Value=value;out.append(p)
    return tuple(out)

def connect(pipe):
    local=uno.getComponentContext()
    resolver=local.ServiceManager.createInstanceWithContext('com.sun.star.bridge.UnoUrlResolver',local)
    ctx=resolver.resolve(f'uno:pipe,name={pipe};urp;StarOffice.ComponentContext')
    return ctx.ServiceManager.createInstanceWithContext('com.sun.star.frame.Desktop',ctx)

def start_office(directory):
    directory=Path(directory).resolve();directory.mkdir(parents=True,exist_ok=True)
    pipe='jingwei_'+uuid.uuid4().hex
    log=(directory/'office.log').open('wb')
    proc=subprocess.Popen(['/usr/bin/libreoffice',f'-env:UserInstallation={(directory/"profile").as_uri()}','--headless','--norestore','--nodefault','--nofirststartwizard',f'--accept=pipe,name={pipe};urp;StarOffice.ComponentContext'],stdout=log,stderr=subprocess.STDOUT)
    try:
        for _ in range(100):
            try:return proc,log,pipe,connect(pipe)
            except Exception:
                if proc.poll() is not None:raise RuntimeError('LibreOffice exited; see office.log')
                time.sleep(.1)
        raise TimeoutError('UNO startup timed out')
    except BaseException:
        proc.terminate();proc.wait(timeout=10);log.close();raise

def stop_office(proc,log):
    proc.terminate()
    try:proc.wait(timeout=10)
    except subprocess.TimeoutExpired:proc.kill();proc.wait()
    log.close()

def load(desktop,path):
    return desktop.loadComponentFromURL(Path(path).resolve().as_uri(),'_blank',0,props(Hidden=True,ReadOnly=False,UpdateDocMode=0,MacroExecutionMode=uno.getConstantByName("com.sun.star.document.MacroExecMode.NEVER_EXECUTE")))

def save(doc,path):
    doc.calculateAll()
    doc.storeAsURL(Path(path).resolve().as_uri(),props(FilterName='Calc MS Excel 2007 XML',Overwrite=True))

def snapshot(doc):
    result={}
    for name in doc.Sheets.ElementNames:
        sheet=doc.Sheets.getByName(name);cursor=sheet.createCursor();cursor.gotoEndOfUsedArea(True);end=cursor.RangeAddress
        if (end.EndColumn+1)*(end.EndRow+1)>20000:raise ValueError('workbook exceeds smoke limits')
        r=sheet.getCellRangeByPosition(0,0,end.EndColumn,end.EndRow)
        result[name]={'values':[list(x) for x in r.getDataArray()],'formulas':[list(x) for x in r.getFormulaArray()]}
    return result

def errors(doc):
    found=[]
    for name in doc.Sheets.ElementNames:
        s=doc.Sheets.getByName(name);c=s.createCursor();c.gotoEndOfUsedArea(True);a=c.RangeAddress
        if (a.EndRow+1)*(a.EndColumn+1)>20000:raise ValueError('workbook exceeds smoke limits')
        for y in range(a.EndRow+1):
            for x in range(a.EndColumn+1):
                err=s.getCellByPosition(x,y).getError()
                if err:found.append({'sheet':name,'row':y+1,'column':x+1,'error':err})
    return found
