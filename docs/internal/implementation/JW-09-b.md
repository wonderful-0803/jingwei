# JW-09-b：开发代码提交与指南上线交接

本批接用户“现在部署上线，提交远端”的要求。2026-09-05 用户明确选择 GitHub Pages 公开发布；源码已推送，现接入 Pages 部署。不会创建 Sites 项目或改变 main/Cargo 发布边界；实际上线结果在下文单独记录。

## 已完成

- `bb9ed6333bf6e9595d704dd21a3abe797cb619c2` 已推送 `origin/dev`：包含 d2-b 持久预算运行、d2-c1 审计增额、独立 VitePress 指南、测试与开发记录。
- 推送前 fetch 确认无其他设备的新提交；推送后远端核验成功。`main` 仍为 `fd1afc749e96bae60236446df369f1aa632e6f6c`，未创建 tag、未发布 Cargo 包。
- 本机完整基线 `results/baseline/20260904-235511-146`：9/9 检查成功；14 项 workspace 测试、242 项独立契约测试和 12 项指南 doctest，共 268 项。
- Markdown 渲染测试 4/4；根路径与 `/jingwei/` 构建产物各验证 14 个 HTML、485 个本地资源/链接/锚点、3 个实际搜索查询，未发现内部源码混入。
- [远端指南 CI](https://github.com/wonderful-0803/jingwei/actions/runs/33892514296) 已在 Ubuntu 成功完成，精确对应上述提交。这验证的是指南构建，不是 Rust 的 Linux 全量基线。
- 所有层级的 target、下载依赖 node_modules 均未上传。`docs/guide/src/action-step.md` 中原有的名称空格修改保留在本机工作区，未纳入提交；远端指南从提交中的原文构建。

## GitHub Pages 发布约定

已核验仓库为公开仓库，当前 GitHub 账户 wonderful-0803 有管理权限；本批开始前 Pages 和 github-pages 环境都不存在。用户确认采用 GitHub Pages，授权公开发布指南。

以 Actions 为发布源，只部署 `docs/guide/target/pages`，base 为 `/jingwei/`；构建与部署分为两个 job，仅 wonderful-0803/jingwei 仓库 dev 的 push/手动运行可部署，PR/main/fork 保持只读验证。github-pages 环境仅允许 dev 分支，部署 job 串行执行且不取消进行中的部署。不要直接选择仓库根目录或 `/docs` 进行目录式发布，以免内部资料混入站点。

工作流仍仅在 dev；未为手动入口而修改 main。首次和日常发布由相关 push 触发，若默认分支尚无工作流，不宣称 workflow_dispatch 已可从界面使用。

本批不改变 Cargo 交付边界；后续预算开发仍从 [JW-04 计划](JW-04-plan.md)的 d2-c2 开始。本批未实施新的预算功能。
