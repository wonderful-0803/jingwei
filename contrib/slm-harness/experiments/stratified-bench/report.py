import json
from pathlib import Path
from collections import Counter
LABELS={'reshape_structure':'结构重排/展开','conditional_logic':'条件逻辑','lookup_matching':'查找/匹配/关联','filter_sort_deduplicate':'筛选/排序/去重/删除','aggregation_statistics':'汇总/计数/统计','date_time':'日期时间','text_processing':'文本处理','numeric_calculation':'数值计算'}
def collect(root):
 root=Path(root);selection=json.loads((root/'selection.json').read_text());expected=set(selection['ids']);out={'expected_per_model':50,'selection_seed':selection['seed'],'models':{}}
 for model in ['9b','27b']:
  rows=[json.loads(p.read_text()) for p in (root/model).glob('*/metrics.json')]
  ids=[r['id'] for r in rows]
  if len(ids)!=len(set(ids)) or not set(ids)<=expected:raise ValueError('duplicate or unexpected result IDs')
  def stats(items):return {'completed':len(items),'passed':sum(r['passed'] for r in items),'delivered':sum(r['delivery_completed'] for r in items),'end_to_end_passed':sum(r['passed'] and r['delivery_completed'] for r in items)}
  out['models'][model]={**stats(rows),'seconds':sum(r['elapsed_seconds'] for r in rows),'model_calls':sum(r['model_calls'] for r in rows),'categories':dict(Counter(r['category'] for r in rows)),'by_official_type':{k:stats([r for r in rows if r['instruction_type']==k]) for k in selection['official_type_counts']},'by_task_category':{k:stats([r for r in rows if r['primary_category']==k]) for k in selection['primary_category_counts']}}
 return out

def make_report(root,output):
 root=Path(root);summary=collect(root);s=json.loads((root/'selection.json').read_text());lines=['# 新版harness：分层随机50题双模型对比','', '两个模型共用同一组50题和随机顺序；整表25、单元格25，排除先前已测50题。固定种子20260921，按公开题意辅助分层；主任务标签由人工核对，非官方分类。','', '| 模型 | 已完成 | 数值评分通过 | 正式交付 | 两者均通过 | 调用次数 | 逐题耗时合计 |','|---|---:|---:|---:|---:|---:|---:|']
 for model,r in summary['models'].items():lines.append(f"| {model} | {r['completed']}/50 | {r['passed']}/50 | {r['delivered']}/50 | {r['end_to_end_passed']}/50 | {r['model_calls']} | {r['seconds']/60:.1f}分钟 |")
 for field,title in [('by_official_type','官方类型'),('by_task_category','题意辅助分类')]:
  lines+=['',f'## {title}','','| 类型 | 9B数值评分通过/已完成 | 27B数值评分通过/已完成 |','|---|---:|---:|']
  for k in summary['models']['9b'][field]:
   a=summary['models']['9b'][field][k];b=summary['models']['27b'][field][k];lines.append(f"| {LABELS.get(k,k)} | {a['passed']}/{a['completed']} | {b['passed']}/{b['completed']} |")
 lines+=['','## 失败类型','']
 for model,r in summary['models'].items():lines.append(f"- {model}: `{r['categories']}`")
 lines+=['','## 口径与限制','', '- 模型是Qwen3.5-9B Q8_0与Qwen3.8-27B UD-IQ3_S；量化不同，不是纯参数规模对比。','- 新版profile：最多8步、共享纠错2次、检查最多2次、预留2步；上下文32768、输出4096、T0、seed7、无thinking；LibreOffice26.8.0.3。','- 数值评分来自独立重放、引擎重算和gold比较；正式交付来自宿主完成检查。两者分开，不把文件有效当作答案正确。','- 50题覆盖8种主操作；另有13题含样式要求、19题提及公式。现有评分器不完整检查样式、公式必须性、图表或宏实现，不能把数值通过率称为完整产品成功率。没有实际创建图表的题目。','- 这是一组类型平衡的覆盖样本，分布与原400题不同；不作为全集准确率估计。新旧样本不同，也不能把与旧前50题的成绩差直接归因于harness改动。','- 题目清单在推理前冻结，未按模型输出替换题目；原基线结果保留。','',f'证据目录：{root}。清单selection.json、版本manifest.json、逐题result.json/journal/llm-trace.jsonl、汇总progress.json和complete.json。']
 Path(output).write_text('\n'.join(lines)+'\n');return summary
