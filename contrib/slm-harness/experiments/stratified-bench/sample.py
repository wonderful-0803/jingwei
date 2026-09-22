"""Instruction-only auxiliary taxonomy; no workbook or hidden-answer access."""
import random,re
from collections import Counter,defaultdict
PATTERNS={
 'formatting_chart':r'conditional formatting|background colo[ur]|font colo[ur]|highlight(?:ing|ed)? (?:cells|rows)|create (?:a |an )?(?:bar |line |pie |scatter )?chart',
 'date_time':r'\bdates?\b|\btimes?\b|\bmonths?\b|\byears?\b|\bhours?\b|\bdays?\b|\bduration\b',
 'text_processing':r'\bextract|\bstrings?\b|\bcharacters?\b|\bsubstring|\bconcatenat|\btext\b|\bwords?\b|\bprefix|\bsuffix',
 'lookup_matching':r'\blookup|\bvlookup|\bxlookup|\bhlookup|\bmatching\b|\bmatch\b|\bcorresponding\b|\bretriev|\blook up\b',
 'aggregation_statistics':r'\bsum\b|\bsumming\b|\bcount\b|\baverage\b|\btotal\b|\bmedian\b|\bminimum\b|\bmaximum\b|\bfrequency\b|\baggregate|\bgroup(?:ing)? by\b',
 'filter_sort_deduplicate':r'\bfilter|\bsort|\bduplicates?\b|\bunique\b|\bdelete\b|\bremove\b|\bblank\b|\bempty\b',
 'reshape_structure':r'\btranspose|\bpivot|\bunpivot|\bmerge|\bcombine|\bconsolidat|\brearrang|\breorganiz|\bexpand|\bcopy|\bmove|\bsplit|\binsert',
 'conditional_logic':r'\bcriteria\b|\bconditions?\b|\bif\b|\bdepending\b|\bbased on\b',
 'numeric_calculation':r'\bcalculat|\bpercent|\bround|\bmultiply|\bdivid|\bsubtract|\bdifference|\brank|\bnumber|\bratio\b',
}
def labels(task):
 text=task['instruction'].lower()
 return [k for k,p in PATTERNS.items() if re.search(p,text)] or ['other']
def select(tasks,excluded,seed=20260921,per_type=25):
 ids=[str(t['id']) for t in tasks]
 if len(ids)!=len(set(ids)):raise ValueError('duplicate task IDs')
 rng=random.Random(seed);result=[]
 for level in ['Cell-Level Manipulation','Sheet-Level Manipulation']:
  eligible=[t for t in tasks if t['instruction_type']==level and str(t['id']) not in excluded]
  if len([t for t in eligible if not t.get('exclude')])<per_type:raise ValueError('insufficient eligible tasks in '+level)
  buckets=defaultdict(list)
  for t in sorted(eligible,key=lambda t:str(t['id'])):
   tags=labels(t);buckets[tags[0]].append(dict(t,primary_category=tags[0],auxiliary_tags=tags))
  for group in buckets.values():rng.shuffle(group)
  counts=Counter()
  for _ in range(per_type):
   available=sorted(k for k,v in buckets.items() if v)
   minimum=min(counts[k] for k in available)
   category=rng.choice([k for k in available if counts[k]==minimum])
   result.append(buckets[category].pop());counts[category]+=1
 rng.shuffle(result)
 # Some Verified metadata explicitly flags an unusable gold pair. Preserve the
 # randomized order and replace flagged picks within their type/category.
 picked={str(t['id']) for t in result}
 for index,item in enumerate(result):
  if not item.get('exclude'):continue
  candidates=[t for t in tasks if t['instruction_type']==item['instruction_type'] and not t.get('exclude') and str(t['id']) not in excluded|picked and labels(t)[0]==item['primary_category']]
  if not candidates:raise ValueError('no valid metadata replacement within stratum')
  replacement=rng.choice(sorted(candidates,key=lambda t:str(t['id'])))
  tags=labels(replacement)
  result[index]=dict(replacement,primary_category=tags[0],auxiliary_tags=tags,replacement_for=str(item['id']))
  picked.add(str(replacement['id']))
 return result
