"""Finite operation policy for synthetic workbooks. No arbitrary formulas/code."""
import re

def address(value):
    if not isinstance(value,str):raise ValueError('range must be text')
    m=re.fullmatch(r'([A-Z]{1,2})([1-9][0-9]{0,3})(?::([A-Z]{1,2})([1-9][0-9]{0,3}))?',value)
    if not m:raise ValueError('invalid or unsupported A1 range')
    def col(s):
        n=0
        for ch in s:n=n*26+ord(ch)-64
        return n-1
    a,b,c,d=m.groups();x1,y1=col(a),int(b)-1;x2,y2=col(c or a),int(d or b)-1
    if x2<x1 or y2<y1:raise ValueError('reversed range')
    if (x2-x1+1)*(y2-y1+1)>20000:raise ValueError('range too large')
    return x1,y1,x2,y2

def contained(inner,outer):
    a,b,c,d=address(inner);x,y,z,w=address(outer)
    return x<=a<=c<=z and y<=b<=d<=w

def validate_request(r,p,revision):
    if not isinstance(r,dict) or set(r)!={'operation','sheet','range','expected_revision','parameters'}:raise ValueError('invalid operation envelope')
    if type(r['expected_revision']) is not int or r['expected_revision']!=revision:raise ValueError('stale workbook revision')
    if not p['allow_mutation']:raise ValueError('workbook is read-only')
    if r['sheet']!=p['sheet']:raise ValueError('sheet outside authorized scope')
    q=r['parameters'];op=r['operation']
    shapes={'remove_empty_rows':set(),'deduplicate':{'keys'},'sort':{'column','descending'},'group_sum':{'group_by','sum_column','output_sheet'},'set_formula':{'function','source_range','destination'}}
    if op not in shapes or not isinstance(q,dict) or set(q)!=shapes[op]:raise ValueError('unknown action or parameters')
    if op!='set_formula' and not contained(r['range'],p['range']):raise ValueError('target outside authorized scope')
    if op=='deduplicate':
        if not isinstance(q['keys'],list) or not 1<=len(q['keys'])<=4 or not all(isinstance(x,str) and x.strip() for x in q['keys']):raise ValueError('invalid deduplication keys')
    if op=='sort' and (not isinstance(q['column'],str) or type(q['descending']) is not bool):raise ValueError('invalid sort parameters')
    if op=='group_sum':
        if not isinstance(q['group_by'],list) or not 1<=len(q['group_by'])<=2 or not all(isinstance(x,str) for x in q['group_by']) or not isinstance(q['sum_column'],str):raise ValueError('invalid grouping')
        if q['output_sheet'] not in p['allow_output_sheets']:raise ValueError('output sheet not authorized')
    if op=='set_formula':
        if r['range']!=q['destination']:raise ValueError('formula target range must equal destination')
        if q['function'] not in ['SUM','AVERAGE','COUNT','MIN','MAX']:raise ValueError('function not allowed by this experiment')
        if not contained(q['source_range'],p['range']):raise ValueError('formula reference outside authorized scope')
        if q['destination'] not in p['formula_targets']:raise ValueError('formula destination not authorized')
        a,b,c,d=address(q['destination'])
        if (a,b)!=(c,d):raise ValueError('formula destination must be a cell')
    return r
