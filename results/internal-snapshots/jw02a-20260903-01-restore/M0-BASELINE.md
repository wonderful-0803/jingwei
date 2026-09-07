# Jingwei v0.1 — M0 基线与实施准备

| 项目 | 状态 |
| --- | --- |
| 日期 | 2026-09-03 |
| 关联任务 | JW-00；为 JW-09 文档工程准备输入 |
| PRD 范围 | 用户已同意逐步实施 |
| M0 状态 | 公开交付策略已确认；Rust/MSVC 环境就绪，5 项基线检查全部通过；内部验证资产维护方式等决策尚待落实，未完成冻结 |
| 当前变更 | 更新 PRD/M0、指南路径规则与 docs/README.md；新增被忽略的内部检查脚本和日志，生成被忽略的 Cargo.lock；未改功能代码、Cargo 配置、Git 配置或发布状态 |

## 1. 已确认的产品与工作边界

- 构建通用的小模型 Agent 框架，不构建某个具体业务项目。
- 保持机制与策略分离、宿主显式组合、默认实现可替换和清楚的恢复/权限边界。
- v0.1 包括 PRD 的 F1–F7；F6 的 P1 优先级不意味着可以不交付。
- 开发指南与代码同版本交付，文档更新从开发过程中开始。
- 用户已明确：指南可以纳入当前仓库，但与源码隔离；独立示例和测试不公开交付。指南中的必要 API 教学片段作为文档保留，不提供独立示例工程或测试套件。
- 当前先执行 M0，不跳过基线验证直接扩展公共 API。
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
| 最低 Rust 版本声明 | `Cargo.toml` 的 `rust-version = "1.85"`；存在已确认的源码不兼容证据，见第 4 节 |
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

在仓库根目录可通过 `& .\scripts\check-baseline.ps1 -Jobs 4` 重跑同一检查集合；它依赖本机已有工具链和依赖缓存。该脚本及 results 日志保持内部使用、被 Git 忽略，尚未配置私有备份。新终端应可直接调用 cargo；已打开的旧终端可以重开，或临时使用 `C:\Users\74090\.cargo\bin\cargo.exe`。

## 4. 工具链与版本决策输入

### 4.1 已确认问题：当前源码不符合 Rust 1.85 声明

例如 [AgentRuntime](C:/Users/74090/Documents/ChatGPT/DSH学习/jingwei/crates/runtime/agent/src/lib.rs:780) 存在 `if condition && let Some(...)`；SessionRuntime 也使用类似的 `let` 链。

Rust 官方将该语法列为 Rust 1.88.0 的稳定功能，且要求 edition 2024。因此，不改动相关源码时，当前 `rust-version = "1.85"` 不能成立。这是源码/语言版本核对结果，不是已运行 Rust 1.85 编译器得到的测试结果。[Rust 1.88.0 发布说明](https://blog.rust-lang.org/2025/06/26/Rust-1.88.0/)

**建议的处理顺序，尚未修改 manifest：**

1. 先准备仓库当前固定的 Rust 1.96.0，并执行基线编译/测试，不在基线核验阶段随意升级到最新 stable。
2. 记录解析后的依赖版本和编译结果，再确定真正支持的最低工具链。
3. v0.1 可优先采用“开发和支持下限统一为已验证的 1.96.x”策略；若要求支持更旧版本，则增加最低版本 CI，而不是仅调整数字。
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

根清单当前只有 workspace，没有根 package；各 package 根目录位于 crates，根 docs 与它们是分离的。当前各 crate 继承 workspace 的 exclude，其中已列出 docs、examples、tests、benches、fixtures 等目录，本次不额外改 Cargo 清单。

这只是静态结构与规则核对，不等于已验证 `.crate` 内容。Cargo 的 include/exclude 控制打包清单，官方建议用 `cargo package --list` 核对；还应从真实包解包构建，验证不依赖 docs 或私有文件。[Cargo 包内容规则](https://doc.rust-lang.org/cargo/reference/manifest.html#the-exclude-and-include-fields)、[cargo package](https://doc.rust-lang.org/cargo/commands/cargo-package.html)

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

已建立 docs/README.md 记录目录边界；docs/guide 为后续指南内容区域。本次尚未创建指南站构建配置、安装文档工具或宣称指南已完成。

## 7. 接下来可以按顺序执行的工作

1. 以本次已通过的 Rust 1.96.0/Windows 基线为依据，落实工具链/MSRV 声明、锁文件维护和实际发布版本策略；本次没有自动修改 `rust-version = "1.85"`。
2. 按已确认的“指南独立公开、示例/测试内部保存”策略，落实内部资产维护方式及可能存在的上游发布流程，设计现有 18 项测试的迁移与覆盖保留方式。
3. 在已调整的指南路径规则上建立指南骨架及构建说明，推进 JW-09；内部验证和 Cargo 包清单检查不因资产不公开而省略。
4. 明确两个待评测模型配置和本地后端版本。先列资源需求和下载清单，获得授权后再下载，不凭 GPU 型号直接宣布模型可用。
5. 更新 M0 清单，达到门槛后进入 M1/JW-01 的契约 ADR；当前无已观察到的本机基线构建失败需要先修复。

## 8. M0 完成清单

- [x] PRD 范围已获用户同意，实施顺序已明确。
- [x] 当前分支、完整 HEAD、origin、crate 数量、版本与工具链声明已记录。
- [x] 用户已确认指南独立入仓库、示例/测试不公开交付的策略。
- [x] 已窄化开放指南路径，保留示例/测试/CI 的忽略边界。
- [x] 当前硬件与可用工具环境已核验，不夸大模型性能或工具就绪程度。
- [x] Rust 1.85 声明与源码语法的不兼容已有证据。
- [ ] 内部验证资产的存储、备份/版本管理及现有内嵌测试迁移方案落实。
- [x] Rust 与 Windows 构建前置环境就绪，基线 fmt/check/test/clippy/doc 实际运行并全部通过。
- [ ] 工具链/MSRV、依赖复现和版本政策已确定。
- [ ] 指南工程方案构建验证与 Cargo 包/公开源码内容验收完成。
- [ ] 模型/后端的候选配置和资源准备路径已明确，真实评测仍在后续阶段完成。

**结论：开发环境准备与本机基线验证已完成；M0 仍有版本政策、内部资产维护和指南工程等事项，尚未进入 M1。**
