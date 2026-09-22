import json,sys
from pathlib import Path
root=Path(sys.argv[1]).resolve();tool=sys.argv[2];args=json.load(sys.stdin)
if tool=='inspect_text':
    result={'ok':True,'text':(root/'input.txt').read_text()[:10000]}
elif tool=='submit_text':
    text=args['content']
    if not isinstance(text,str) or len(text)>10000:raise ValueError('invalid content')
    (root/'answer.txt').write_text(text)
    result={'ok':True,'validated':True,'note':'Text artifact registered; content correctness is not checked.'}
else:raise ValueError('unknown tool')
print(json.dumps(result))
