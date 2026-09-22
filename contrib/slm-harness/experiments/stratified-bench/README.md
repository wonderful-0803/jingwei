# E006 新版harness分层随机50题

用户授权：9B Q8_0与27B UD-IQ3_S各跑同组50题，非官方前缀顺序，尽量覆盖任务类型。

固定选择：artifacts/spreadsheetbench/stratified50-v2/selection.json。官方只有整表/单元格两类，各选25。排除先前已评测50个ID；先按公开instruction启发式主类型做组内随机+类别均衡取样，seed20260921，然后打乱最终顺序。全部选中题意经人工检查，修正主标签但不替换/重排任何题，原启发式标签留在sampling_primary_category。无gold参与选样。

最终八类：筛选排序去重删除12、查找匹配9、结构重排6、条件逻辑6、汇总统计6、日期5、文本3、数值3。样式要求13题、提及公式19题。原评分器不完整检查这些约束；没有实际创建图表题。非概率总体估计，不把50题准确率外推400题，也不能与旧前50题直接归因比较harness效果。

执行：`experiments/spreadsheetbench/.venv/bin/python -u experiments/stratified-bench/run.py`。

- 先9B50题，再27B相同顺序50题；v2profile、steps8/corrections2/inspection2/reserve2、ctx32768/output4096/T0/seed7/thinkingnone。
- 复用已验证的task_run、沙箱、LibreOffice重算和官方值比较；没有更改harness/模型提示词/评分器。
- 固定manifest记录源码、二进制、题目数据、模型digest与profile；复制固定二进制，每题前核对源码未变。运行中不要修改实验依赖。
- 逐题result/journal/trace是完整证据；metrics.json是轻量完成标记。progress.json区分数值评分、宿主交付和两者均通过。
- 发生HTTP、引擎、grader或宿主进程基础设施错误立即停止，不记为完成；保留证据，复跑时旧尝试会归档。模型自身错答/步数或纠错耗尽属于正常失败。
- SIGTERM/SIGINT请求当前题结束后停止；不强杀共享Ollama。process.json含真实PID。恢复使用同一命令；manifest必须相同，已完成题跳过。若另有模型加载则拒绝启动，不擅自卸载。
- 全部100题完成自动生成complete.json与docs/reports/spreadsheetbench-stratified50-v2-2026-09-21.md，并释放E006；启动后无需人工逐题继续。

选样质检：公开metadata将56274标注为gold下拉值不一致，按同官方类型/辅助日期类别固定种子替换为44017；推理前完成，替换记录保留在selection.json。未读取gold内容决定选样。独立审查提出重复启动状态污染和评分源码哈希缺口，均已修复。
