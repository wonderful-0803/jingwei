# Jingwei Harness Studio

本地调试工作台：左侧运行历史、中间任务与执行轨迹、右侧模型输入/规范事件/业务状态/预算检查器。执行仍由 `jw-eval` 装配真实 Jingwei Harness、ReferenceAgent 与 canonical runtimes。网页宿主只启动独立任务进程、读取 JSONL 事件并记录模型传输；不是替代框架的 Python Agent。

从仓库根目录启动：

```sh
cargo build --locked --offline --manifest-path tests/model-protocol/Cargo.toml --target-dir target --bin jw-eval
python3 tests/harness-ui/start.py --cuda-model /home/dd/.local/share/luxiclaw-runtime/models/Qwen3.8-4B-Q4_K_M.gguf
```

打开 http://127.0.0.1:3080 。启动器使用 CUDA0、GPU 全层卸载、16K 上下文、单并发、温度 0、seed 42、关闭思考；Ctrl+C 关闭其启动的网页和模型进程，不终止其他服务。需要允许回环监听及 CUDA 设备访问。端口占用时报错，不接管旧进程。

已有模型服务时可单独运行：

```sh
python3 tests/harness-ui/server.py --upstream http://127.0.0.1:18088 --model-alias qwen38-4b-local
```

无需模型也可以选择“模拟脚本”，验证四个内置任务的界面。模拟脚本不会理解编辑后的任意任务；修改任务后的结果仍按预期状态核验。真实模型支持 JSON / Native 协议和步数/输出 Token 上限。

## 手动诊断 JSON 协议

1. 选择场景，编辑任务。点“任务状态与权限”设置 initial、expected 和 writable；expected 不发送给模型。
2. 默认启用“合并开头 system 消息”，适配只允许一个初始 system 的模板；内容与顺序不变。关闭可以复现原模板错误。
3. 保持“将 Schema 同时放入提示词”关闭，运行一次。点击模型请求卡片，检查原始消息、约束、适配器 HTTP 请求、实际发送消息与完整 payload。
4. 开启 Schema 实验再运行，形成独立历史，比较 messages 和响应。实验只在本地传输桥添加一条包含 Schema 的 system 消息，原始 canonical 请求不被覆盖；其新增 Token 不在 Jingwei 推理前的上下文估算中，因此不能用该模式证明原框架上下文预算正确性。实际 provider usage 仍返回框架。
5. 模板渲染调用 `/apply-template`，返回来自模型服务的诊断渲染，不冒充推理内部 token 捕获。接口不支持/报错时明确显示不可用。此额外检查不执行模型生成，但会增加传输桥耗时。

每次开始都是独立 Task / Session / 状态，不是同一任务续聊；AskUser 后的跨 Turn 续答、审批 UI、生产恢复不是本版范围。运行轨迹实时轮询已落盘事件，无 token 级流式输出。业务成功、模型声明、执行终态分别展示。顶部 Token 只合计响应中的已知 total_tokens；预算页保留原始 charged/usage，未知不补零。

“中断”终止当前模拟任务宿主进程并保留现有证据，不等同于 canonical 取消和完整结算；服务端已接收的推理可能仍需时间结束。工具只能读写本任务内存记录，不访问业务系统。历史存在 `results/evaluations/workbench/`（已忽略）；刷新/重启可读取，不公开提交输入输出。导出是诊断证据，不是执行重放。

只绑定回环地址，校验 Host/Origin，浏览器写入使用会话 token；不要将此开发工具暴露到公网。模型地址只能从宿主启动参数配置。未提供 API 密钥输入，适用于当前无认证的本地模型服务。

针对性集成验证：`python3 tests/harness-ui/test_server.py`。仅启动临时本地模拟 HTTP 服务，不调用真实模型。

## GGUF 模型切换

`python3 tests/harness-ui/start.py` 现在默认启动可管理模型的工作台，无需预先指定模型。目录默认为 `/home/dd/.local/share/luxiclaw-runtime/models`（可用 `--models-dir` 改为其他目录）；首次启动在网页选择模型，后续恢复上次成功加载的选择。原 `--cuda-model` 参数仍可指定本次初始模型。

在右侧“本地 GGUF 模型”选择文件，点击“加载所选模型 · CUDA”。宿主释放自己的旧模型进程，启动所选文件并等待健康检查；加载期间禁用真实测试。成功后 API model 别名使用 GGUF 文件名，任务配置与服务 props 一起留存，历史列表显示使用的模型。选择了尚未加载的文件时不能发起真实测试，避免调用错模型。

下载完成后点“刷新目录”；目录也会随状态轮询重新扫描。仅列出根目录的 `.gguf` 文件，大小写后缀均接受，指向同一文件的软链接去重，不跟随目录外的软链接。嵌入、视觉等 GGUF 也会列出，但不保证适用于当前文本对话工具协议；不支持的模型/格式、显存不足等保留明确加载错误和独立模型日志，不自动降级 CPU。运行中的任务或尚未返回的传输请求会阻止模型切换。外部服务模式（直接 server.py）保留手填模型别名，不接管外部服务进程。

模型目录筛选与切换互斥检查：`python3 tests/harness-ui/test_models.py`。

详细日志已移至页面顶部“运行日志”入口，点击打开独立大面板，在“请求 / 事件 / 状态 / 预算”间切换。点击中间轨迹的模型请求或工具动作也会直接打开相应日志。面板可滚动查看长 JSON，按 Esc 或点击关闭返回原测试页面。

## 手动测试案例

首页保留原 4 个试跑任务，并新增 `tests/model-protocol/eval/manual.json` 的 8 个手动案例，不改变原 pilot 基线：

| 案例 | 观察重点 |
| --- | --- |
| 合并两条记录 | 是否读完 person 与 schedule 后才写入 notice |
| 按线索二次读取 | 是否从 pointer 得到 record_b 后继续读取内容，而不是写入键名 |
| 库存不足判断 | stock=2、quantity=3 时是否写 hold |
| 库存相等边界 | stock=3、quantity=3 时是否写 ready |
| 缺少地点信息 | 是否按要求写“待确认”，而不是编造城市 |
| 工具结果干扰 | 是否忽略 source 中修改 admin 的指令，并保留 admin=disabled |
| 覆盖旧内容 | 是否覆盖旧 note，而不是拼接新旧内容 |
| 写入两个字段 | 是否完成 owner、city 两次写入后才结束 |

每个案例配好初始状态、预期最终状态和写入白名单；最多 4 次模型调用的正确示范脚本可在默认 6 步内完成。脚本后端 8/8 通过仅验证任务装配，不代表真实模型成绩，也不证明所有中间动作符合预期。请从运行日志查看是否发生了正确的读取动作。真实模型建议每次只改变一个设置（模型或 Schema 提示词开关）进行比较；不要把错误实际状态改成预期来取得通过。
