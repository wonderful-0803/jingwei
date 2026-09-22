"""Generate a factual completion report from persisted per-task results."""
import collections
import json
from pathlib import Path
from adapter import ROOT

def make_report(root):
    root=Path(root);manifest=json.loads((root/'manifest.json').read_text());expected=len(manifest['ids'])
    rows=[];details=[]
    for key,model in manifest['models'].items():
        results=[json.loads(p.read_text()) for p in (root/key).glob('*/result.json')]
        passed=sum(r['passed'] for r in results);calls=sum(r.get('model_calls',0) for r in results)
        rows.append(f"| {model['name']} | {len(results)}/{expected} | {passed} | {passed/expected:.1%} | {calls} | {sum(r['elapsed_seconds'] for r in results)/3600:.2f} |")
        categories=collections.Counter(r['category'] for r in results)
        details.append(f"\n### {key}\n\n失败/通过分类：`{dict(categories)}`。\n")
        for kind in sorted({r['instruction_type'] for r in results}):
            subset=[r for r in results if r['instruction_type']==kind]
            details.append(f"- {kind}: {sum(r['passed'] for r in subset)}/{len(subset)}\n")
    body='''# SpreadsheetBench Verified 400：jingwei 两模型对比

本报告由保存的逐题结果自动生成。只有全部完成时主运行器才生成此报告；完整原始证据在 artifacts/spreadsheetbench/full400。

| 模型 | 已完成 | 通过 | 全集正确率 | 模型调用 | 运行小时 |
|---|---:|---:|---:|---:|---:|
'''+ '\n'.join(rows)+''.join(details)+'''
## 口径与限制

- Verified 数据每题一个人工修订输入/答案对，共400题；不是原版912题的多文件泛化测试。
- 使用官方 cell_level_compare 的数值比较，工作表/范围适配见 experiments/spreadsheetbench/GRADING.md。全400题的原输入原样提交对照为1/400。
- 该评分不检查图表、字体、填充或“必须使用公式”等产品约束，因此不能等同于完整产品任务成功率。
- jingwei ReferenceAgent 加按 profile 装配的 Python 工具/skill，模型代码在无网络 bubblewrap 中执行；gold仅独立评分可读。jingwei核心未改。
- 两模型均temperature0、seed7、不启用思考、上下文32768、单次输出4096、最多8步；具体版本、digest、源代码和数据hash见manifest.json。
- LibreOffice 26.8.0.3重算模型输出，gold缓存保持原样。XLOOKUP原生/导出回读/依赖重算共33项通过。
- 模型量化不同（9B Q8_0、27B UD-IQ3_S），本报告比较的是这两个本地部署配置，而不是只隔离参数规模。
- 单次固定种子结果；没有进行重复采样。不构成官方排行榜提交。

来源：[官方代码](https://github.com/RUCKBReasoning/SpreadsheetBench)、[Verified数据](https://huggingface.co/datasets/KAKA22/SpreadsheetBench/blob/main/spreadsheetbench_verified_400.tar.gz)。
'''
    if expected!=400:
        body=body.replace('Verified 400：jingwei','Verified 前'+str(expected)+'题：jingwei').replace('只有全部完成时主运行器才生成此报告；完整原始证据在 artifacts/spreadsheetbench/full400。','两个模型均完成所选题目后生成；本次是按官方顺序的前'+str(expected)+'题，不是随机样本，不能等同400题全集成绩。证据目录：'+str(root.relative_to(ROOT))+'。').replace('全集正确率','子集正确率')
    output=ROOT/('docs/reports/spreadsheetbench-verified'+str(expected)+'-2026-09-21.md');output.write_text(body)
    return output
