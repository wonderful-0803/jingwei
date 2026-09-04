# JW-09-b：开发代码提交与指南上线交接

本批接用户“现在部署上线，提交远端”的要求。源码推送已完成；上线正在等待托管方式与访问范围选择。未创建 Sites 项目，也未启用 GitHub Pages，不存在已确认的在线指南地址。

## 已完成

- `bb9ed6333bf6e9595d704dd21a3abe797cb619c2` 已推送 `origin/dev`：包含 d2-b 持久预算运行、d2-c1 审计增额、独立 VitePress 指南、测试与开发记录。
- 推送前 fetch 确认无其他设备的新提交；推送后远端核验成功。`main` 仍为 `fd1afc749e96bae60236446df369f1aa632e6f6c`，未创建 tag、未发布 Cargo 包。
- 本机完整基线 `results/baseline/20260904-235511-146`：9/9 检查成功；14 项 workspace 测试、242 项独立契约测试和 12 项指南 doctest，共 268 项。
- Markdown 渲染测试 4/4；根路径与 `/jingwei/` 构建产物各验证 14 个 HTML、485 个本地资源/链接/锚点、3 个实际搜索查询，未发现内部源码混入。
- [远端指南 CI](https://github.com/wonderful-0803/jingwei/actions/runs/33892514296) 已在 Ubuntu 成功完成，精确对应上述提交。这验证的是指南构建，不是 Rust 的 Linux 全量基线。
- 所有层级的 target、下载依赖 node_modules 均未上传。`docs/guide/src/action-step.md` 中原有的名称空格修改保留在本机工作区，未纳入提交；远端指南从提交中的原文构建。

## 上线待确认

已只读核验仓库为公开仓库，当前 GitHub 账户 wonderful-0803 有管理权限，Pages 尚未启用。已向用户询问：GitHub Pages 公开访问，或 Sites 暂时仅本人访问。不得把尚未回复的默认选项当作公开授权。

若选择 GitHub Pages：以 Actions 为发布源，只部署 `docs/guide/target/pages`，base 为 `/jingwei/`；将构建与部署分为两个 job，仅 dev 的可信 push/手动运行可部署，PR 保持只读验证。不要直接选择仓库根目录或 `/docs` 进行目录式发布，以免内部资料混入站点。上线后核对远端任务成功和 HTTPS 页面可读，再补写真实 URL。

若选择 Sites：创建并持久保存唯一站点标识，只提交和打包独立指南来源与静态产物，默认保持本人访问；根据实际返回的访问策略执行发布。不能为了部署自动扩大访问范围。

两种路径都不改变 Cargo 交付边界；后续预算开发仍从 [JW-04 计划](JW-04-plan.md)的 d2-c2 开始。本批未实施新的预算功能。
