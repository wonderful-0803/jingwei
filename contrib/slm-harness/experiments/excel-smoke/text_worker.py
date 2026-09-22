from pathlib import Path
import json,sys
root=Path(sys.argv[1]);tool=sys.argv[2];args=json.load(sys.stdin)
if tool!='text_stats' or args:raise ValueError('invalid request')
text=(root/'input.txt').read_text()
print(json.dumps({'ok':True,'validated':True,'lines':len(text.splitlines()),'words':len(text.split()),'characters':len(text)},ensure_ascii=False))
