# Jingwei v0.1 — M0 基线与实施准备

| 项目 | 状态 |
| --- | --- |
| 日期 | 2026-09-03 |
| 关联任务 | JW-00；为 JW-09 文档工程准备输入 |
| PRD 范围 | 用户已同意逐步实施 |
| M0 状态 | 构建基线通过；MSRV 与私有验证目录已明确，M1 首批协议开发已开始；模型与异机备份等后续门槛继续跟踪 |
| 当前变更 | 已实现结构化协议词汇/预检、能力查询、TaskId/StepId；同步 Cargo/MSRV 与指南，新增私有测试及本地快照；未修改 Git 配置、提交、推送或发布 |

## 1. 已确认的产品与工作边界

- 构建通用的小模型 Agent 框架，不构建某个具体业务项目。
- 保持机制与策略分离、宿主显式组合、默认实现可替换和清楚的恢复/权限边界。
- v0.1 包括 PRD 的 F1–F7；F6 的 P1 优先级不意味着可以不交付。
- 开发指南与代码同版本交付，文档更新从开发过程中开始。
- 用户已明确：指南可以纳入当前仓库，但与源码隔离；独立示例和测试不公开交付。指南中的必要 API 教学片段作为文档保留，不提供独立示例工程或测试套件。
- 已完成 M0 构建基线后进入 M1；依用户“尽快进入开发流程”的要求，无 IO 的契约开发与其余准备事项并行推进，不降低后续验收要求。
- 用户于 2026-09-03 授权准备环境，随后完成 Rust 与 Windows 编译前置工具安装；未下载模型、启动推理服务、创建远端分支、推送或发布。

## 2. 仓库快照

| 项目 | 核验结果 |
| --- | --- |
| 本地目录 | `C:\Users\74090\Documents\ChatGPT\DSH学习\jingwei` |
| origin | `https://github.com/wonderful-0803/jingwei` |
| 当前分支 | `main` |
| 当前 HEAD | `fd1afc749e96bae60236446df369f1aa632e6f6c` |
| 本地 tag | 无；不据此推断远端或包注册表从未发布 |
| workspace | 15 个 crate；edition 2024 |
| 包版本 | workspace 统一声明 `0.1.0` |
| 开发工具链声明 | `rust-toolchain.toml` 固定 `1.96.0` |
| 最低 Rust 版本声明 | 基线为不准确的 `1.85`；现已同步为已验证的 `1.96`，见第 4 节 |
| 核验开始时的工作区 | 功能代码干净；`PRD-v0.1.md` 为此前新增的未跟踪文件 |
| 当前工程性质 | 基线 `.gitignore` 曾自述为 public source mirror；当前仓库为本次代码/指南工作基线。未取得其他私有工程或上游生成流程，不自动迁移它们 |

父目录另有 Git 仓库，本次未变更其配置、索引或提交；所有工作限定于 jingwei 内。

## 3. 本机开发与评测候选环境

通过只读系统查询及 `nvidia-smi` 获取，不含设备序列号、账号或计算机名称。

| 项目 | 核验值 |
| --- | --- |
| 操作系统 | Windows 11 家庭版中文版，64 位，版本 `10.0.26200` |
| CPU | Intel Core i7-14650HX，16 核、24 逻辑处理器 |
| 物理内存 | 系统报告约 15.7 GiB，可按 16 GB 级设备描述 |
| 独立 GPU | NVIDIA GeForce RTX 4060 Laptop GPU |
| GPU 总显存 | `nvidia-smi` 报告 8188 MiB；不是当前可用显存 |
| NVIDIA 驱动 | `581.29` |
| 集成显卡 | Intel UHD Graphics |

该机器是可用的评测候选设备，不代表某个模型已验证可装入显存或满足延迟目标。模型上下文、KV cache、并发与其他程序占用均会影响运行边界。

### 3.1 安装前的工具调查

| 工具/路径 | 本次结果 | 解释 |
| --- | --- | --- |
| 指定 Git for Windows | 可调用；使用 `C:\Program Files\Git\cmd\git.exe` | 不改用应用内置 Git |
| `rustup` / `rustc` / `cargo` | PATH 中均未找到 | 尚不能执行 Rust 编译、测试、rustdoc |
| 用户 `.cargo\bin` / `.rustup\toolchains` | 常见路径不存在 | 不能排除其他自定义安装路径，但当前会话无可用工具链 |
| `cl` / `cmake` | PATH 中未找到 | 单凭 PATH 不能证明所有 C++ 工具不存在，结合下行路径判断 |
| Visual Studio 安装目录 / `vswhere.exe` / Windows Kits 10 | 检查的常见路径均不存在 | 当前未发现 Windows MSVC 开发前置组件 |
| `llama-server` / `ollama` | PATH 中未找到 | 未扫描用户模型目录，也未探测或启动本地模型服务 |
| `nvidia-smi` | 可调用 | 只用于记录硬件，不触发推理 |

Windows MSVC 目标需要链接器、库和 Windows API 导入库；Rust 官方列出的前置组件包括 MSVC 工具及 Windows SDK。安装方式与所需权限在真正安装时单独确认。[Rustup MSVC prerequisites](https://rust-lang.github.io/rustup/installation/windows-msvc.html)

### 3.2 2026-09-03 环境安装结果

仅从 Rust 和 Microsoft 官方站点下载安装程序。执行前核对 Rustup 官方 SHA-256，以及 Microsoft 安装程序的有效 Authenticode 签名。安装程序位于被忽略的 results/environment-20260903/installers，未进入源码交付。

| 项目 | 已验证的安装结果 |
| --- | --- |
| Rustup | `1.29.1 (d95a37b6a 2026-08-13)` |
| Rust | `1.96.0 (ac68faa20 2026-05-25)`；host 为 `x86_64-pc-windows-msvc` |
| Cargo | `1.96.0 (30a34c682 2026-05-25)` |
| Rustfmt / Clippy | `rustfmt 1.9.0-stable` / `clippy 0.1.96` |
| Rust 组件 | cargo、rustc、rust-std、rust-docs、rustfmt、clippy，使用 default profile |
| Visual Studio Build Tools 2022 | `17.14.37614.0`；安装退出码 0，完整且可用 |
| MSVC | 工具集 `14.44.35207`，x64 linker 已存在 |
| Windows SDK | SDK 库目录 `10.0.26100.0`；安装器报告成功 |
| 重启 | 安装禁用了自动重启；安装完成状态不要求重启 |
| PATH | 已核对实际用户 PATH 包含 `.cargo\bin`；旧终端需要重新打开，内部检查脚本仅为自身进程补充 PATH |
| 磁盘 | 安装前 C 盘约 166.1 GiB 可用；安装后、基线编译期间约 158.9 GiB 可用，后续编译产物还会占用空间 |

MSVC 安装选取 `Microsoft.VisualStudio.Component.VC.Tools.x86.x64`、`Microsoft.VisualStudio.Component.Windows11SDK.26100` 及英文语言包，由官方安装器解析依赖；未安装完整 Visual Studio IDE，未添加模型运行环境。

安装依据：[Rust 官方安装说明](https://rust-lang.org/tools/install/)、[Microsoft 安装参数](https://learn.microsoft.com/en-us/visualstudio/install/use-command-line-parameters-to-install-visual-studio?view=vs-2022)、[Build Tools 组件清单](https://learn.microsoft.com/en-us/visualstudio/install/workload-component-id-vs-build-tools?view=vs-2022)。

### 3.3 依赖和基线验证

- `cargo fetch` 成功，从 crates.io 解析并下载 213 个第三方包；加上 15 个 workspace crate，完整 metadata 为 228 个包。
- 根 `Cargo.lock` 已生成，仍遵循现有 Git 忽略规则；SHA-256 为 `CCAC6143247D485AC4E5CE5FE118F92E5B4338EC1517407C4A3866A8845CCA01`。该本地快照用于本次验证，不代表私有备份/长期复现策略已经落实。
- 关键解析结果：jsonschema 0.50.1、reqwest 0.12.28、tokio 1.53.1、serde 1.0.229、serde_json 1.0.151、uuid 1.26.0、thiserror 2.0.20；未修改清单中的依赖约束。
- 新增内部脚本 scripts/check-baseline.ps1，顺序执行 fmt、check、test、严格 Clippy 和 rustdoc。除格式检查外均使用 `--locked --offline`，编译并发默认 4，不修改源文件。
- 检查日志和 summary.json 位于 results/baseline/20260903-212448-013；5 项检查全部退出码为 0，结果如下。
- 未进行模型加载、性能基线、显存适配或推理服务健康检查；mdBook 与指南站工程尚未安装/构建。

| 基线检查 | 结果 | 本次耗时 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 通过，没有自动格式化源码 | 0.38 秒 |
| `cargo check --workspace --all-targets --all-features --locked --offline --jobs 4` | 15 个 crate 通过 | 44.05 秒 |
| `cargo test --workspace --all-features --locked --offline --jobs 4` | 18 项测试通过，0 失败、0 忽略；当前 doc-tests 为 0 项 | 63.93 秒 |
| `cargo clippy --workspace --all-targets --all-features --locked --offline --jobs 4 -- -D warnings` | 通过 | 5.14 秒 |
| `cargo doc --workspace --all-features --no-deps --locked --offline --jobs 4` | 15 个 crate 的 rustdoc 生成成功，日志无警告 | 3.81 秒 |

这些耗时包含本次本机缓存状态，不是框架性能评测。基线仅覆盖当前 Windows x64/MSVC + Rust 1.96.0 与上述锁文件，不证明最低版本、跨平台、Cargo 发布包或 v0.1 新功能已经通过验收。

在仓库根目录可通过 `& .\scripts\check-baseline.ps1 -Jobs 4` 重跑检查；它依赖本机工具链、依赖缓存与私有测试 workspace。脚本现已扩展到 8 项检查，新增内容见第 9 节；脚本与日志保持内部使用。新终端应可直接调用 cargo；旧终端可以重开，或临时使用 `C:\Users\74090\.cargo\bin\cargo.exe`。

## 4. 工具链与版本决策输入

### 4.1 已修正问题：基线源码不符合原 Rust 1.85 声明

例如 [AgentRuntime](C:/Users/74090/Documents/ChatGPT/DSH学习/jingwei/crates/runtime/agent/src/lib.rs:780) 存在 `if condition && let Some(...)`；SessionRuntime 也使用类似的 `let` 链。

Rust 官方将该语法列为 Rust 1.88.0 的稳定功能，且要求 edition 2024。因此，不改动相关源码时，当前 `rust-version = "1.85"` 不能成立。这是源码/语言版本核对结果，不是已运行 Rust 1.85 编译器得到的测试结果。[Rust 1.88.0 发布说明](https://blog.rust-lang.org/2025/06/26/Rust-1.88.0/)

**已执行的版本决策：**

1. 按固定的 Rust 1.96.0 完成基线编译/测试，没有升级到浮动的最新 stable。
2. 已将 workspace 的 `rust-version` 改为 `1.96`；rust-toolchain.toml 保持 1.96.0 并显式声明 minimal profile、Rustfmt/Clippy。
3. 以已通过的本机 1.96.0 作为当前支持下限；若要求支持更旧版本，需增加最低版本验证，不能仅调整数字。提高声明后启用的 Clippy 检查发现 JSONL 一处取模写法，已等价替换为 is_multiple_of，完整回归通过。
4. 至少 1.88 的语法下限不代表完整依赖集合必然支持 1.88；完整 MSRV 必须以实际验证为准。

Rust 1.96.0 是仓库明确指定、已有官方发布的版本。[Rust 1.96.0 发布说明](https://blog.rust-lang.org/2026/05/28/Rust-1.96.0/)

### 4.2 功能里程碑与包版本

- 继续用“v0.1”指代 PRD 功能里程碑。
- 基线保留现有 `0.1.0`，不提前 bump、不创建 tag、不发布包。
- 需要确认已有公开发布历史；实际发版版本应结合已发布 API 和 M1 的兼容性评审确定。
- 不根据“本地没有 tag”推断可以覆盖现有版本，也不提前承诺未完成的 API 变更一定向后兼容。

## 5. 主开发仓库与公共交付边界

### 5.1 已确认的交付决策

用户于 2026-09-03 明确：开发指南可以纳入仓库，但需与源码隔离，以便将来发布到 Cargo/crates.io；示例和测试不公开交付。

据此确定：

- 框架生产源码继续位于各 crate；官方参考 Agent 属于框架实现，不属于被排除的示例项目。
- 指南输入位于 workspace 根目录的 docs/guide，不进入任何 crate 根目录，不加入 workspace members。
- 只开放 docs/README.md 和 docs/guide；其他内部文档继续默认忽略。
- 独立示例、tests、benches、fixtures、假 provider/工具和原始评测材料不进入公共 Git 或 `.crate` 包，内部验收照常执行。
- 指南可保留自包含的 API 用法片段，但不链接或 include 私有示例、测试套件或数据；内部流程从指南抽取片段校验，而非使指南构建依赖内部工程。
- 原始内部评测报告不自动公开；公开指南只陈述有验证依据的支持范围和限制。

本决定明确了交付边界；内部工程的私有存储与版本管理尚待落实，也没有据此创建或推送任何远端仓库。

### 5.2 当前路径规则

以 `git check-ignore --no-index` 核验下列策略：

| 路径类别 | 预期 Git 行为 |
| --- | --- |
| docs/README.md、docs/guide/src/SUMMARY.md、docs/guide/book.toml | 可跟踪 |
| docs 下其他内部设计资料 | 忽略 |
| docs/guide/book、target、.cache 构建目录 | 忽略 |
| examples、tests、benches、fixtures、运行结果 | 继续忽略 |
| .github 现有内部工程配置 | 保持原规则；本次未授权公开测试工作流 |

PRD 与本记录仍为未提交的本地规划资料；本次没有 `git add`、提交、推送、创建 tag 或发布操作。

### 5.3 Cargo 包边界与尚未完成的发布核验

根清单当前只有 workspace，没有根 package；各 package 根目录位于 crates，根 docs 与它们是分离的。各 crate 继续继承原有 exclude 规则，新增私有测试使用独立 workspace，不加入主 workspace。

已对 15 个 crate 运行 `cargo package --list --allow-dirty --offline`，未发现独立 docs、tests、examples、fixtures 等路径。此检查不等于实际 `.crate` 解包构建，也无法剥离既有内嵌测试；生成的 Cargo.lock 等包元数据仍可出现。后续还须从真实包解包构建，验证不依赖私有文件。[Cargo 包内容规则](https://doc.rust-lang.org/cargo/reference/manifest.html#the-exclude-and-include-fields)、[cargo package](https://doc.rust-lang.org/cargo/commands/cargo-package.html)

后续验收区分：公共 Git 文件清单、独立指南构建依赖、各 Cargo 包清单与包内源码内容。不能只修改 `.gitignore` 就宣称不存在测试泄漏，也不能通过 `include` 意外重新纳入被忽略资产。

### 5.4 基线内嵌测试的处理

现有多个 src 文件包含 `#[cfg(test)] mod tests`，例如 core 的事件词汇和 plugin 的实现文件。它们本来就随源码存在；`.gitignore` 和目录级 exclude 无法只隐藏文件中的这些段落。

- 本次保留所有现有源码与测试，没有删除测试逻辑，也不改写公开 Git 历史。
- 基线编译/测试可运行后，记录已有覆盖，再将内部测试逻辑迁移至不公开的验证工程或经明确设计的内部测试机制。
- 迁移属于 JW-08/发布检查的必办事项，接口/存储方案在 M0/M1 明确；迁移后公共源码不得依赖缺失的私有 include 文件，不为测试公开不必要的内部 API。
- v0.1 新公开源码和 Cargo 包必须检查是否仍含内部测试套件；已有历史内容无法仅靠本次忽略规则撤回。
- 内部资产不能仅靠 `.gitignore` 留在工作区：必须有可恢复的私有保存/版本管理方式；本次没有自动创建私有仓库或备份任务。

## 6. 开发指南工程建议

推荐评估 Markdown + mdBook + rustdoc：mdBook 提供 Markdown 文档、搜索、代码高亮和 Rust 示例测试能力，符合当前 Rust 项目的指南需求，不需要借用 LangChain 的 Python API 或云端服务。[mdBook 官方介绍](https://rust-lang.github.io/mdBook/)

该技术栈目前是建议，不表示已经安装、构建或完成视觉验收。固定的构建工具版本应在 JW-09 开始时记录；版本化 URL/目录由发布工程负责，不能把 mdBook 本身误当作完整版本管理服务。

拟议内容分组沿用修订后的 PRD：开始使用、核心概念、构建 Agent、小模型工程、可靠性与安全、扩展开发、验证与排错、参考与迁移。公开内容不依赖内部评测套件或原始数据。

docs/guide/src 已包含概览、结构化模型协议和开发环境说明，4 个 Rust 教学片段由私有工程提取为 doctest 并通过。尚未安装 mdBook 或创建指南站构建配置，不宣称完整指南已完成。

## 7. 接下来可以按顺序执行的工作

1. 继续 JW-02/JW-03：唯一生成协议、版本化模型事件和 schema 校验边界已接入；下一步补齐动作解析/执行闭环与真实后端双模式验证，不把回环 HTTP 视为真实模型验收。
2. 按已确认的“指南独立公开、示例/测试内部保存”策略，落实内部资产维护方式及可能存在的上游发布流程，设计现有 18 项测试的迁移与覆盖保留方式。
3. 随功能接入扩充指南并建立文档站构建工程；落实异机备份、包版本与真实 `.crate` 解包构建验收。
4. 明确两个待评测模型配置和本地后端版本。先列资源需求和下载清单，获得授权后再下载，不凭 GPU 型号直接宣布模型可用。
5. 后续 JW-01 ADR 补齐版本化事件、预算及状态/恢复边界；当前首份 ADR 不代表所有框架契约均已冻结。

## 8. M0 完成清单

- [x] PRD 范围已获用户同意，实施顺序已明确。
- [x] 当前分支、完整 HEAD、origin、crate 数量、版本与工具链声明已记录。
- [x] 用户已确认指南独立入仓库、示例/测试不公开交付的策略。
- [x] 已窄化开放指南路径，保留示例/测试/CI 的忽略边界。
- [x] 当前硬件与可用工具环境已核验，不夸大模型性能或工具就绪程度。
- [x] Rust 1.85 声明与源码语法的不兼容已有证据。
- [ ] 内部验证资产的存储、备份/版本管理及现有内嵌测试迁移方案落实。
- [x] Rust 与 Windows 构建前置环境就绪，基线 fmt/check/test/clippy/doc 实际运行并全部通过。
- [x] 工具链/MSRV 已同步为 1.96.0/1.96，两个锁文件随本地快照保留。
- [ ] 依赖快照的异机维护与实际发布版本政策已完成。
- [ ] 指南工程方案构建验证与 Cargo 包/公开源码内容验收完成。
- [ ] 模型/后端的候选配置和资源准备路径已明确，真实评测仍在后续阶段完成。

**结论：已进入 M1 首批实现；准备事项不阻塞纯协议开发，但没有被撤销或视为已验收。**

## 9. M1 首批实现与验证（2026-09-03，历史阶段）

本节保留 JW-02-a 当时的代码位置、兼容决策与验证证据；当前接口已由 ADR-0002 / JW-02-b 直接替换，不再保留旧文本 API。

当前阶段验证：results/baseline/20260903-222711-594，8/8 检查通过，14 个保留内嵌用例、52 个私有用例和 4 个指南 doctest 共 70 个通过；15/15 包清单检查通过。详细改动、历史用例替换与未完成门槛见 docs/internal/implementation/JW-02-b.md。未进行真实模型推理、公开提交或发布。

- 实现记录：docs/internal/implementation/JW-02-a.md；首份 ADR：docs/internal/adr/0001-model-protocol.md。
- 新增协议位于 crates/llm/llm/src/protocol.rs，公共 facade 通过 jingwei::llm 导出。新增 TaskId/StepId 和 provider/gateway 能力元数据；旧 ChatMessage、规范事件及文本推理入口保持兼容。
- 私有契约工程：tests/model-protocol，独立 Cargo workspace，publish=false；公共源码不 include 该目录。既有内嵌测试尚未迁移。
- 最终验证：results/baseline/20260903-214732-347，8/8 检查通过；18 项既有测试、25 项新契约测试和 3 项指南 doctest 通过。首次严格 Clippy 失败日志保留在 20260903-214557-139，未隐藏失败记录。
- 打包清单记录：results/package-lists/jw02a-20260903.json；15/15 包清单检查通过，真实包构建与旧内嵌测试迁移仍待完成。
- 本地恢复快照：results/internal-snapshots/jw02a-20260903-01.zip；含本批选定的源码、私有测试、脚本、锁文件和文档，54 个文件解包后逐个 SHA-256 比对通过。快照 SHA-256：`F8B8D9D9193D51E65A00392E90EE0082461675FDC602EB51949A4ED38792EA6C`。这是同盘恢复副本，不是异机备份、远端仓库或包含 Git 历史的完整镜像。
- JW-02-a 阶段 root Cargo.lock SHA-256：`E8D41B77E1D1B116D22C062CC07DEE4D26DD7EC623CC27DB48265C4AE8504349`；私有测试锁文件：`5F4E9401B5A30C87AAACC4A3D1450390BA82DE6047633681FC4E6A7159636730`。第 3 节保留安装时的初始快照结果，两者阶段不同。
