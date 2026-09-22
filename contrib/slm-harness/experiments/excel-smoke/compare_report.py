"""Offline comparison; never contributes hidden answers to model requests."""
from pathlib import Path
import json,statistics
BASE=Path(__file__).resolve().parents[2]/'artifacts/excel-smoke/compare-9b-27b'
LABELS={'empty':'删空行','dedup':'按键去重','sort':'排序','group':'分组汇总','formula':'SUM公式','chain':'三步组合','overwrite':'覆盖拒绝','scope':'越界拒绝','text':'文本统计'}
RUNTIME=['evaluate.py','worker.py','policy.py','office.py','proxy.py','src/main.rs','spreadsheet.json','text.json','text_worker.py','skills/spreadsheet.md','skills/text.md','compare_model.py']
def analyze(directory):
    root=BASE/directory;env=json.loads((root/'environment.json').read_text());results=json.loads((root/'model-results.json').read_text());cases={}
    for r in results:
        case=root/('model-'+r['case']);traces=[json.loads(x) for x in (case/'llm-trace.jsonl').read_text().splitlines()]
        actions=[];usage={'prompt_tokens':0,'completion_tokens':0};errors=[]
        for t in traces:
            for k in usage:usage[k]+=t['response'].get('usage',{}).get(k,0)
            if t['status']!=200:errors.append(t['response']);continue
            actions.append(json.loads(t['response']['choices'][0]['message']['content']))
        agent=json.loads((case/'agent-report.json').read_text());state=json.loads((case/'state.json').read_text()) if (case/'state.json').exists() else None
        tool_replies=[]
        for t in traces:
            for m in t['request']['messages']:
                if m['role'] not in ('system','assistant') and 'jingwei_tool_result' in m.get('content','') and m['content'] not in tool_replies:tool_replies.append(m['content'])
        cases[r['case']]={**r,'actions':actions,'usage':usage,'final_text':agent.get('final_text'),'disposition':agent.get('disposition'),'state':state,'tool_replies':tool_replies,'http_errors':errors,'host_seconds':agent.get('elapsed_seconds')}
    excel=[c for k,c in cases.items() if k!='text'];positive=[cases[k] for k in ['empty','dedup','sort','group','formula','chain']]
    return {'directory':directory,'environment':env,'cases':cases,'passed':sum(c['pass'] for c in cases.values()),'positive_passed':sum(c['pass'] for c in positive),'refusal_passed':sum(cases[k]['pass'] for k in ['overwrite','scope']),'text_passed':cases['text']['pass'],'inspect_first':sum(bool(c['actions']) and c['actions'][0].get('name')=='inspect_workbook' for c in excel),'all_originals_preserved':all(c['checks']['original_unchanged'] for c in excel),'all_protected_preserved':all(c['checks']['protected_unchanged'] for c in excel),'model_calls':sum(c['model_calls'] for c in cases.values()),'excel_mean_seconds':statistics.mean(c['seconds'] for c in excel),'usage':{k:sum(c['usage'][k] for c in cases.values()) for k in ['prompt_tokens','completion_tokens']}}

def main():
    nine=analyze('qwen35-9b-v2');large=analyze('qwen38-27b-v2')
    for p in RUNTIME:assert nine['environment']['source_sha256'][p]==large['environment']['source_sha256'][p],p
    assert nine['environment']['binary_sha256']==large['environment']['binary_sha256']
    # Assert the first submitted request differs only by model; catches accidental skill/schema drift.
    for k in LABELS:
        requests=[]
        for item in [nine,large]:
            t=json.loads((BASE/item['directory']/('model-'+k)/'llm-trace.jsonl').read_text().splitlines()[0]);r=t['request'];r.pop('model');requests.append(r)
        assert requests[0]==requests[1],k
    summary={'9b':nine,'27b':large,'identical_runtime_hashes':True,'identical_initial_requests_except_model':True}
    (BASE/'comparison.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2))
    rows=[]
    for k,name in LABELS.items():
        a=nine['cases'][k];b=large['cases'][k]
        rows.append(f"| {name} | {'通过' if a['pass'] else '失败'} | {'通过' if b['pass'] else '失败'} | {a['seconds']:.1f} / {b['seconds']:.1f} | {a['model_calls']} / {b['model_calls']} |")
    details=[]
    for k,c in nine['cases'].items():
        details.append(f"### 9B：{LABELS[k]}\n\n动作：`{json.dumps(c['actions'],ensure_ascii=False)}`\n\n最终状态：{c['disposition']}\n\n最终回答：{c['final_text']}\n")
    report=f'''# Qwen 9B Q8 与 27B IQ3：相同Excel套件对比

模型分别为 `hf.co/unsloth/Qwen3.5-9B-GGUF:Q8_0` 与 `hf.co/unsloth/Qwen3.8-27B-GGUF:UD-IQ3_S`。不同模型版本和量化精度，不能把差异单独归因于参数量。

## 固定条件

同一RTX3090、Ollama、LibreOffice、jingwei二进制；相同题目、合成数据、隐藏答案、工具schema、skill、权限与评分。核对了12个执行相关源码hash和二进制hash；9例首个HTTP请求除model外逐项相等。

两款模型均从Ollama空载状态分别预热，warmup不计入任务耗时；本轮先9B后27B，每模型每例只测一次，没有为9B优化提示。temperature=0、seed=7、reasoning_effort=none、max_tokens=1024、最多12步；都使用相同的开头system合并及schema明文提示。实际context见environment.json（预热请求32K）；宿主8K只是Soft估算。

## 结果

| 指标 | 9B Q8 | 27B IQ3 |
|---|---:|---:|
| 业务正确 | {nine['positive_passed']}/6 | {large['positive_passed']}/6 |
| 安全拒绝结果 | {nine['refusal_passed']}/2 | {large['refusal_passed']}/2 |
| 非Excel文本 | {int(nine['text_passed'])}/1 | {int(large['text_passed'])}/1 |
| 首先检查工作簿 | {nine['inspect_first']}/8 | {large['inspect_first']}/8 |
| 模型请求数 | {nine['model_calls']} | {large['model_calls']} |
| 8个Excel任务平均结束耗时 | {nine['excel_mean_seconds']:.2f}秒 | {large['excel_mean_seconds']:.2f}秒 |
| 预热耗时 | {nine['environment']['warmup_seconds']:.2f}秒 | {large['environment']['warmup_seconds']:.2f}秒 |

“结束耗时”包含失败/等待用户，因此失败较快不能解读为完成任务效率更高。表格任务时间含LibreOffice初始化和离线验证，文本时间仅为宿主执行，分别报告。

预热后整卡显存：9B `{nine['environment']['gpu_loaded']}`；27B `{large['environment']['gpu_loaded']}`。这是固定时点占用，不是精确峰值。两个模型的实际size_vram/digest/context_length均保留原始API响应。

| 任务 | 9B | 27B | 耗时秒（9B/27B） | 调用数（9B/27B） |
|---|---|---|---:|---:|
{chr(10).join(rows)}

原文件与保护数据：9B全部保留={nine['all_originals_preserved'] and nine['all_protected_preserved']}；27B全部保留={large['all_originals_preserved'] and large['all_protected_preserved']}。安全性与业务完成必须分开理解。

## 主要差异

9B的5个业务失败都在首次调用的范围参数处停止：把`Data!A1:D9`传给单独的range字段，同时又填写sheet=Data，工具只接受不带表名的A1地址。随后它转为ask_user，部分解释误把范围错误归因为列名或表头不确定。三步组合甚至没有进入第二步，因此这些结果不能分别证明去重、汇总算法或多步推理做不到。

9B越界例同样先填`Data!H1`，再以ask_user请求确认；虽然原文件安全，但不算正常完成的越界拒绝。9B唯一成功写入的SUM例按明确的公式参数生成真实公式，重算检查通过。覆盖例由工具拒绝后能说明原因。

因此本轮明确暴露的是在当前工具契约下的参数格式与流程遵从差距。若宿主进一步明确range的pattern/示例、强制inspect前置，9B是否改善需要新的对照实验，不能根据这次结果预断。

## 兼容性修正

9B初始直接复用旧接口时，9例全部遇到HTTP500，其GGUF模板要求system只出现于开头且只一条。这是生成前的模板错误，不计为能力成绩；保留在qwen35-9b目录。

实验proxy新增可选merge_system，将连续开头system内容拼接为单条，保留协议和skill全部文本；两个模型均使用此模式重新测试。默认旧行为不变。3项合并回归与11项策略测试通过，jingwei核心未改。两模型的实际对比目录是qwen35-9b-v2 / qwen38-27b-v2。

## 9B轨迹与失败归因

{chr(10).join(details)}

## 限制与后续

这是8行合成表、6项业务、2项拒绝和1个文本任务的一次小样本对比；没有大表、图表、日期规范化和任意公式。27B先前已用这些题调试接口，这不是盲测保留集。模型9B发生的range语法或流程选择错误保留为成绩，未为它变更工具接口。

正确性以实际输出和真实LibreOffice重算为准；拒绝文本还需人工检查语义，关键词评分不能视为通用裁判。若后续为9B增加更明确的range pattern/示例或宿主inspect前置门禁，应作为新实验，同时重测两模型，不覆盖本轮。

证据：[完整比较数据](../../artifacts/excel-smoke/compare-9b-27b/comparison.json)、[9B结果](../../artifacts/excel-smoke/compare-9b-27b/qwen35-9b-v2/model-results.json)、[27B结果](../../artifacts/excel-smoke/compare-9b-27b/qwen38-27b-v2/model-results.json)。每例目录包含请求/响应、jingwei journal、原始工作簿与输出（如有）。PR仍暂停。
'''
    target=BASE.parents[2]/'docs/reports/excel-smoke-compare-9b-27b-2026-09-21.md';target.write_text(report)
    print(json.dumps({name:{k:v for k,v in data.items() if k not in ('cases','environment')} for name,data in [('9b',nine),('27b',large)]},ensure_ascii=False,indent=2))
if __name__=='__main__':main()
