# E001：真实 Excel 冒烟实验

用户授权继续测试；本实验独立于 Claude 的 H001 总体审阅，全部代码位于 experiments/excel-smoke，不是待发布的正式框架实现。

- [x] 固定实验范围：清除空行、按键去重、排序、分组求和、SUM公式、越界/覆盖拒绝，加一个非Excel工具任务。
- [x] 先验证范围、版本、白名单和覆盖策略，再通过真实UNO执行器对照证明任务可执行。
- [x] 用现有 jingwei ReferenceAgent + JSON action + 工具网关 + JSONL日志运行同一组任务。
- [x] 通过实验专用本地转发层固定 temperature=0、seed=7、reasoning_effort=none；记录实际请求与Ollama usage，不修改核心模型接口。
- [x] 每例用独立工作簿副本和LibreOffice profile；检验原文件哈希、范围外数据、真实重算与导出再打开。
- [x] 隐藏答案只在离线评分进程；Agent只获得题目、工具schema和显式技能说明。
- [x] 报告实际成功/失败、调用次数、耗时；区分工具执行器问题与模型选择错误。

实验限制：task profile显式选择已注册插件与skill，不代表正式的动态目录或运行中扩展已完成。当前SUM公式以受限函数/范围参数表达，不声称任意Excel公式生成。暂不覆盖图表、日期和XLOOKUP。没有训练/调参，不用人工替模型补动作。

写范围：本实验目录、artifacts/excel-smoke、docs/reports/excel-smoke*，协作状态按锁更新。jingwei源码只读。

结果：见 docs/reports/excel-smoke-2026-09-21.md。run-01、run-03与最终run-04均保留；最终业务结果9/9，先检查流程遵从6/8。当前是已完成的有限实验，不代表完整产品功能或正式装配层完成。
