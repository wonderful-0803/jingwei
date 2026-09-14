# JW-08-a：内部评测入口

日期：2026-09-14。基于 dev / 574947f；继续 wonderful-0803 提交。无公共 crate/API 修改。

在独立 `tests/model-protocol` 验证工程新增 `jw-eval` 二进制。配置文件定义 backend/model/endpoint/协议/重复次数/步数/输出 Token/超时及设备、权重、量化、模板记录；任务文件相对配置解析。支持 `--case` 精确筛选，输出目录必须全新，避免覆盖已有证据。示例与操作说明见 [运行器说明](../../../tests/model-protocol/eval/README.md)。

四个试跑任务覆盖文档复制/字段提取、订单有库存/无库存分支。模型通过同一 ReferenceAgent、canonical runtimes、受授权模拟工具和 JSONL Session 执行。每次任务使用独立状态及 Harness；业务验证比较完整最终状态，同时要求回合 Completed 且 shutdown 无错误，模型声明不能独立证明成功。fake 脚本不发给真实 provider。

输出 manifest、逐次 results.jsonl、summary 和每次独立 Session 日志。保留状态不匹配、运行异常及收尾错误；单任务失败继续后续任务，基础设施写入失败则返回非零。报告含 TaskRunReport 原始预算/计数、耗时、受拒写入次数与未知内存字段。真实 token usage 不自行补零。原始运行输出可存入已忽略的 `results/evaluations/`，任务定义仍由 dev 跟踪。

## 针对性验证

- `cargo test --locked --offline --manifest-path tests/model-protocol/Cargo.toml --target-dir target --bin jw-eval`：3 项通过，覆盖两协议四任务、虚假完成/越权写入以及配置拒绝。
- `python3 tests/model-protocol/eval/verify_cli.py target/debug/jw-eval`：通过；验证命令行四任务、精确筛选、不覆盖旧结果、失败继续并汇总，以及临时本地 HTTP 服务经真实 OpenAi 适配器完成三步任务。沙箱禁止监听 socket，HTTP 检查在获准的提升执行中进行，不调用真实模型。
- 同一二进制 Clippy `-D warnings` 通过；Rust 格式化完成。未重复全量基线。

## 未宣称完成的部分

四任务和模拟服务验证只证明驱动通路，不是模型效果报告。40 任务、真实两模型三轮、采样/模板冻结、性能指标完整采集、消融和 AT-F5 验收后续实施。内存本批分别记未知；上下文为软估算，模型采样沿用服务端默认。JW-07-e 继续暂缓，不宣称完整恢复验收。
