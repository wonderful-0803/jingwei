"""Render the frozen smoke evidence; does not execute or alter model results."""
import json,statistics
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2];base=ROOT/'artifacts/excel-smoke'
results=json.loads((base/'run-04/model-results.json').read_text())
controls=json.loads((base/'run-04/control-results.json').read_text())
labels={'empty':'删空行','dedup':'按订单号去重','sort':'金额降序排序','group':'按地区汇总','formula':'SUM公式与重算','chain':'删空行→去重→汇总','overwrite':'已有目标禁止覆盖','scope':'拒绝范围外写入','text':'非Excel文本统计'}
rows=[];peak=0;usage={'prompt_tokens':0,'completion_tokens':0};actions={};refusals={}
for r in results:
    traces=[json.loads(x) for x in (base/'run-04'/('model-'+r['case'])/'llm-trace.jsonl').read_text().splitlines()]
    sequence=[]
    for trace in traces:
        u=trace['response'].get('usage',{});peak=max(peak,u.get('prompt_tokens',0))
        for k in usage:usage[k]+=u.get(k,0)
        action=json.loads(trace['response']['choices'][0]['message']['content']);sequence.append(action.get('name',action['action']))
    actions[r['case']]=sequence
    rows.append(f"| {labels[r['case']]} | {'通过' if r['pass'] else '失败'} | {r['seconds']:.1f} | {r['model_calls']} | {'output.xlsx' if r.get('published') else '无工作簿发布'} |")
    if r['case'] in ['scope','overwrite']:
        a=json.loads((base/'run-04'/('model-'+r['case'])/'agent-report.json').read_text());refusals[r['case']]=a.get('final_text')
summary={'passed':sum(r['pass'] for r in results),'total':len(results),'control_passed':sum(r['pass'] for r in controls),'mean_seconds':statistics.mean(r['seconds'] for r in results),'model_calls':sum(r['model_calls'] for r in results),'usage':usage,'peak_request_prompt_tokens':peak,'sequences':actions,'refusals':refusals}
(base/'summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2))
report=f'''# Qwen + jingwei Excel 冒烟结果（2026-09-21）

最终修订套件 **{summary['passed']}/{summary['total']}** 通过，其中6项业务操作、2项拒绝场景、1项非Excel任务；执行器确定性对照 **{summary['control_passed']}/8**。这是同一小样本上经过接入修正的开发集结果，不是独立保留集、稳定成功率或完整产品能力边界。

## 最终结果

| 任务 | 结果 | 耗时秒 | 模型调用 | 输出 |
|---|---|---:|---:|---|
{chr(10).join(rows)}

共 {summary['model_calls']} 次模型请求，平均每例 {summary['mean_seconds']:.1f} 秒。Ollama累计计费统计为输入 {usage['prompt_tokens']} tokens、输出 {usage['completion_tokens']} tokens，输入含多轮重复上下文；单次请求最大输入 {peak} tokens。Excel耗时含临时LibreOffice启动/评分，文本耗时来自宿主，不能作为严格等口径性能比较。

数据为8条合成记录，包含空记录、重复键、0值和带前导零的字符串编号。独立答案检验原数据、去重与顺序、汇总、保护工作表、额外单元格和额外工作表。SUM不只查公式文本：LibreOffice导出重开后值应660，将源值100临时改101后重算应661，验证改动不保存。

## 接入过程中发现的问题

1. **run-01：0/9完成**。Schema用于约束解码，却没有作为可读工具目录进入提示。模型会盲猜列名、跳过检查、重复操作、遗漏验证。其完成门禁又把正常拒绝视为失败，不能将这轮0/9归结为Excel模型能力。
2. **差分探针**：只合并system消息仍直接写入；明确提供相同schema后首个动作正确为inspect_workbook。实验proxy将schema同时用于提示和解码。没有更换模型或放宽参数白名单。
3. **run-03：业务5/6**，非Excel1/1。公式失败源于工具range语义歧义：模型把F2作为写入目标，但旧执行器要求它仍是数据源区域。覆盖用例虽安全，拒绝理由也受该歧义影响，不能算正确的覆盖识别。
4. **run-04**将公式写目标range=destination与读源source_range分开授权，再完整重跑；其余操作保持数据源范围含表头。业务黄金答案未改。文本第二轮起要求最终结果用JSON提供，以便明确检验2行、5词、32字符。
5. 独立审阅发现评分漏项并补齐：必须Completed才能发布；完整Data和工作表集合；正确文本答案；拒绝需核对原因。额外写入F2或Summary的两个评分回归通过。11项策略单测通过；公式接口修复有先失败后通过的回归记录。

## 安全拒绝人工核验

覆盖例最终回答：{refusals.get('overwrite')}

越界例最终回答：{refusals.get('scope')}

两例都必须无output.xlsx、原文件哈希不变、数据和目标不变。自动拒绝评分仍是固定用例关键词筛查；以上完整回答用于人工核对，不能推广为通用语义判分器。

## 过程遵从的实际边界

8个Excel任务中只有6个先调用inspect_workbook；公式与覆盖例直接调用apply_operation，违反skill要求的先检查。最终结果评分9/9不等于过程遵从9/9。覆盖被执行器实际拒绝，公式经执行器校验后写入；没有额外损害。这说明关键安全检查必须放在宿主/工具中，不能只靠skill。若先检查是产品硬约束，应增加宿主状态门禁，并作为单独指标重新测试。

## 当前能确认的范围

该量化模型在明确的工具目录、有限动作和小表数据条件下，可以选择与串联这些工具完成任务。计算与写文件由确定性的LibreOffice工具完成；本测试不证明模型会自行编写可靠Excel代码或任意公式。

同一个Rust宿主分别装配spreadsheet与text profile；文本例只看到text_stats，表格例只看到3个表格工具。jingwei核心源码未改。当前是受信任配置驱动的任务装配，不是运行中的插件/skill热加载，也未实现桌面Agent全流程。

## 尚不能下结论

- 格式规范化、日期/金额解析、筛选、拆并列、图表、双维汇总、复杂公式和真实用户文件还没覆盖。
- 没有大表、模糊需求、恶意单元格指令、保持复杂格式、复杂跨表依赖、不同随机种子和保留集。
- 本机LibreOffice24.2不覆盖XLOOKUP；本轮未测。
- 单次模型输出1024 tokens、最多12步；宿主8K Soft估算未计proxy注入的schema，Ollama实际context_length=32768。不可宣称严格8K测评。
- 技术校验只保证有限的完整性，不判断业务意图；语义正确由离线答案确认。隔离工作副本不是生产级崩溃恢复事务。

下一步应冻结修正后的接口，用新生成的保留题扩展数据规模、组合深度、格式混杂和否定任务；再逐类增加产品要求中的功能。把多轮规划错误、参数选择错误、宿主适配问题、引擎不支持分开统计。

## 复现与证据

- [运行说明](../../experiments/excel-smoke/README.md)
- [最终模型结果](../../artifacts/excel-smoke/run-04/model-results.json)
- [确定性对照](../../artifacts/excel-smoke/run-04/control-results.json)
- [调用序列与汇总](../../artifacts/excel-smoke/summary.json)
- [模型、引擎、显卡和参数](../../artifacts/excel-smoke/environment.json)
- [提示差分探针](../../artifacts/excel-smoke/prompt-probes.json)
- [首轮结果](../../artifacts/excel-smoke/run-01/model-results.json)
- [第二轮结果](../../artifacts/excel-smoke/run-03/model-results.json)
- [SUM输出](../../artifacts/excel-smoke/run-04/model-formula/output.xlsx)
- [组合任务输出](../../artifacts/excel-smoke/run-04/model-chain/output.xlsx)

每例目录包含input.xlsx、工作副本、候选/发布文件（如有）、llm-trace.jsonl、agent-report.json与jingwei journal。无真实用户表格数据。PR按用户要求保持暂停。
'''
path=ROOT/'docs/reports/excel-smoke-2026-09-21.md';path.parent.mkdir(exist_ok=True,parents=True);path.write_text(report)
print(json.dumps({k:v for k,v in summary.items() if k!='sequences'},ensure_ascii=False,indent=2))
