"""Single controlled tool request. Host fixes task directory and UNO pipe."""
from pathlib import Path
import sys,json,os,hashlib,shutil
from collections import defaultdict
from office import connect,load,save,snapshot,errors
from policy import validate_request,address

def store_state(root,state):
    tmp=root/'state.tmp';tmp.write_text(json.dumps(state,ensure_ascii=False));tmp.replace(root/'state.json')

def apply(doc,r):
    sheet=doc.Sheets.getByName(r['sheet']);region=sheet.getCellRangeByName(r['range']);rows=[list(x) for x in region.getDataArray()]
    op=r['operation'];p=r['parameters']
    if op=='set_formula':
        cell=sheet.getCellRangeByName(p['destination'])
        if cell.getFormula()!='':raise ValueError('destination not empty; overwrite denied')
        formula='='+p['function']+'('+p['source_range']+')';cell.setFormula(formula)
        return {'destination':p['destination'],'formula':formula}
    headers=rows[0];body=rows[1:]
    if len(set(headers))!=len(headers) or any(not isinstance(h,str) or not h for h in headers):raise ValueError('invalid or duplicate headers')
    def idx(name):
        if name not in headers:raise ValueError('unknown column '+name)
        return headers.index(name)
    if any(str(v).startswith('=') for row in region.getFormulaArray()[1:] for v in row):raise ValueError('transforming existing formulas is unsupported in this smoke')
    if op=='remove_empty_rows':body=[row for row in body if any(v!='' for v in row)]
    elif op=='deduplicate':
        cols=[idx(n) for n in p['keys']];seen=set();new=[]
        for row in body:
            key=tuple(row[c] for c in cols)
            if key not in seen:new.append(row);seen.add(key)
        body=new
    elif op=='sort':
        c=idx(p['column']);pop=[r for r in body if r[c]!=''];blank=[r for r in body if r[c]=='']
        if len({isinstance(r[c],str) for r in pop})>1:raise ValueError('mixed sort types require explicit conversion')
        body=sorted(pop,key=lambda r:r[c],reverse=p['descending'])+blank
    elif op=='group_sum':
        name=p['output_sheet']
        if doc.Sheets.hasByName(name):raise ValueError('output sheet already exists; overwrite denied')
        cols=[idx(n) for n in p['group_by']];value=idx(p['sum_column']);groups=defaultdict(float)
        for row in body:
            if not any(v!='' for v in row):continue
            if not isinstance(row[value],(float,int)):raise ValueError('non-numeric value in sum column')
            groups[tuple(row[i] for i in cols)]+=row[value]
        data=[p['group_by']+[p['sum_column']]]+[list(k)+[v] for k,v in sorted(groups.items())]
        doc.Sheets.insertNewByName(name,doc.Sheets.getCount());s=doc.Sheets.getByName(name)
        s.getCellRangeByPosition(0,0,len(data[0])-1,len(data)-1).setDataArray(tuple(tuple(x) for x in data))
        return {'output_sheet':name,'groups':len(groups)}
    else:raise ValueError('operation not implemented')
    count=len(body);padded=rows[:1]+body+[['']*len(headers)]*(len(rows)-count-1)
    region.setDataArray(tuple(tuple(row) for row in padded))
    return {'rows_after':count,'rows_before':len(rows)-1}

def main(root,pipe,tool,arguments):
    policy=json.loads((root/'policy.json').read_text());state=json.loads((root/'state.json').read_text())
    if tool=='inspect_workbook':
        doc=load(connect(pipe),root/'working.xlsx')
        try:return {'ok':True,'revision':state['revision'],'workbook':snapshot(doc),'policy':policy,'blocked':state['blocked']}
        finally:doc.close(True)
    if state['blocked']:return {'ok':False,'error':'transaction already rejected; no publish permitted','revision':state['revision']}
    if tool=='apply_operation':
        try:
            req=validate_request(arguments,policy,state['revision'])
            doc=load(connect(pipe),root/'working.xlsx')
            try:
                summary=apply(doc,req);doc.calculateAll();err=errors(doc)
                if err:raise ValueError('formula errors: '+json.dumps(err))
                save(doc,root/'next.xlsx')
            finally:doc.close(True)
            os.replace(root/'next.xlsx',root/'working.xlsx');state['revision']+=1;state['validated']=False;store_state(root,state)
            return {'ok':True,'revision':state['revision'],'summary':summary}
        except Exception as e:
            state['blocked']=True;state['validated']=False;store_state(root,state)
            return {'ok':False,'error':str(e),'revision':state['revision'],'rolled_back':True}
    if tool=='validate_workbook':
        doc=load(connect(pipe),root/'working.xlsx')
        try:
            doc.calculateAll();err=errors(doc)
            before=json.loads((root/'protected.json').read_text());snap=snapshot(doc)
            if snap.get('Audit')!=before:raise ValueError('protected sheet changed')
            if err:raise ValueError('formula errors')
            original=hashlib.sha256((root/'input.xlsx').read_bytes()).hexdigest()
            if original!=state['input_sha256']:raise ValueError('original workbook modified')
            save(doc,root/'candidate.xlsx')
        finally:doc.close(True)
        doc=load(connect(pipe),root/'candidate.xlsx')
        try:
            doc.calculateAll()
            if errors(doc):raise ValueError('export/reopen formula errors')
        finally:doc.close(True)
        state['validated']=True;store_state(root,state)
        return {'ok':True,'validated':True,'revision':state['revision'],'formula_errors':0,'protected_sheet_unchanged':True,'original_unchanged':True}
    raise ValueError('unknown tool')

if __name__=='__main__':
    try:result=main(Path(sys.argv[1]).resolve(),sys.argv[2],sys.argv[3],json.load(sys.stdin))
    except Exception as e:result={'ok':False,'error':str(e)}
    print(json.dumps(result,ensure_ascii=False))
