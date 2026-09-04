# JW-09-a：VitePress 静态开发指南工程

后续状态：本文件保留指南工程创建时的记录；d2-b/d2-c1/指南已推送 dev，远端指南 CI 已通过。提交与上线状态见 [JW-09-b](JW-09-b.md)，不要将下文历史“未提交”理解为当前状态。

日期：2026-09-04。按用户要求，在 JW-04-d2-c2 前完成本批；后续指南统一采用 VitePress。当前位于 dev，HEAD 仍为 `96f83021ad66466609322a9d223f4c95f525b204`；已有 d2-b、d2-c1 未提交变更原样保留。本批不开发新的预算功能，不提交/推送，不部署公开或私有托管站点。

## 已交付

- docs/guide 内独立 package.json、package-lock.json、.npmrc、.nvmrc 与 .vitepress 配置/主题；不新增 Cargo 成员，不修改 Rust 依赖。
- VitePress 默认文档布局：分组侧栏、页内目录、中文本地搜索、代码复制、深浅色切换、GitHub 编辑入口、开发版本标识与 404 页面。首页直接提供指南内容，不增加营销落地页。
- 既有 11 篇 Markdown（含目录）保留原路径；新增 API 参考与编写/发布说明，合计 13 篇正文/目录、14 个输出 HTML（含 404）。API reference 指向独立 rustdoc 生成方法，不伪造尚未发布版本的 docs.rs 地址。
- Rust/no_run 等代码块在渲染层归一为 Rust 高亮，隐藏 rustdoc 的 `# ` 辅助行；不改写 Markdown，不破坏原有教学片段验证。
- 根路径产物 docs/guide/target/site，以及 `/jingwei/` 项目子路径产物 target/pages。HTML 保留 `.html` 链接，不要求托管服务额外提供无扩展名路由。
- 只有 src 是发布内容来源；内部文档、测试、Cargo 源码、依赖目录和缓存不进入静态目录。node_modules 单独忽略，所有构建输出与缓存置于 target；开发源码和锁文件仍纳入 dev。
- 独立的 .github/workflows/guide.yml：只读仓库权限，构建并检查两种路径，保存静态 artifact；不包含 Pages 写权限、托管配置或部署步骤。本轮没有触发远端 CI。

入口：[指南工程 README](../../guide/README.md)、[公开编写指南](../../guide/src/writing-guide.md)。PRD 的 D2 已更新为用户明确选择的 VitePress；M0 的 mdBook 文字保留为历史建议并注明已被替代。

## 版本与依赖安全

本机 Node.js 24.19.0 / npm 11.17.0。核实 npm 官方 dist-tags 后选用稳定 VitePress 1.6.4，未采用 2.0 alpha。

初始默认依赖扫描发现 3 项漏洞（2 moderate、1 high），涉及 Vite 5/esbuild 的开发服务器。根据 [Vite 官方安全公告](https://github.com/vitejs/vite/security/advisories/GHSA-fx2h-pf6j-xcff)与 npm 版本/peer 范围核验，在指南 package.json 中覆盖 Vite 为 6.4.3，配合 Vue 插件 5.2.4 支持的 Vite 6 范围。覆盖后安装审计为 **0 vulnerabilities**；这不等于未来永久无漏洞。该覆盖超出 VitePress 1.6.4 原始依赖范围，需要保留本批构建回归，未来升级时重新审查，不用 npm audit fix --force 自动切换版本。

.npmrc 禁用安装期生命周期脚本；离线 npm ci 已验证可以使用包内平台二进制完成构建。开发和预览只监听本机回环地址，不开放局域网。官方维护资料见 [VitePress v1 安装说明](https://vuejs.github.io/vitepress/v1/guide/getting-started)、[配置参考](https://vuejs.github.io/vitepress/v1/reference/site-config)、[静态部署说明](https://vuejs.github.io/vitepress/v1/guide/deploy)。

锁文件 SHA-256：

- docs/guide/package-lock.json：`32667AD6AE881461376AF00595692E5A78481445D60F449167A5A95D2A51213D`
- root Cargo.lock：`98E25DEC5A6B84FF014AEA2E935855D9586FC85C782730A2BDFED20748B0B3BB`（未变）
- tests/model-protocol/Cargo.lock：`7E7E8DDD488CE77A5A957F23CDACBA3621C04E8FC0ACE092FE0B8DC3C6666B08`（本批未变）

## 实际验证

| 验证 | 本机结果 |
| --- | --- |
| npm run docs:build | 成功，根路径静态站 |
| npm run docs:build:pages | 成功，/jingwei/ 子路径静态站 |
| node tests/guide-site/check.mjs，两种路径分别执行 | 每种 14 个 HTML、485 个本地资源/链接/锚点核验、3 个实际搜索索引查询通过，未发现源码/内部资产混入 |
| node --test tests/guide-site/markdown.test.mjs | 4/4：隐藏行、属性、转义井号及非 Rust 代码保护 |
| 独立 Rust 指南 doctest | 12/12 通过；未改变正文代码与独立工程 include 路径 |
| 独立指南源码副本 | 仅复制 guide 的配置、src、README 和锁文件到 target/guide-standalone-20260904；无 crates/tests/internal，离线 npm ci 与构建成功，同样通过 14 页/485 链接/3 查询检查 |
| 本机 HTTP | 开发首页 200；静态 /jingwei/ 首页与 budget-grants.html 均 200 |
| Cargo facade 包清单 | 6 项，无 docs/tests/examples/node_modules/target 或 npm 配置混入；不是 .crate 解包消费验收 |

静态搜索检查直接读取构建产物里的 MiniSearch 索引，使用站点相同的中文分词配置，验证“预算”“增额”和 BudgetExecutionLease 的结果。它不是截图或浏览器交互测试；本轮未做手机/键盘/跨浏览器人工验收。已请求在应用中打开本机预览，工具返回 queued；不把队列状态写成用户已看到页面。两个临时预览进程在验证后关闭，可按 README 随时重启。

Windows 沙箱最初阻止 esbuild 读取父目录元数据，静态构建和预览通过获准的本地执行完成；没有改低项目安全设置或对外开放服务。初始失败发生在配置解析阶段，不是 VitePress 页面编译错误。

## 后续与边界

本批完成静态指南工程，不代表完整 F7 教学内容、v0.1 或 F6 已验收。文档站可独立重建，但未发布在线 URL；GitHub workflow 是待运行配置，Linux/跨浏览器仍需在后续环境验证。当前 API reference 是 rustdoc 入口说明，完整版本化托管地址在正式发布时补齐。

返回 [JW-04 计划](JW-04-plan.md)继续 d2-c2：Session/恢复单写者所有权与冻结核验。后续每批维护指南页面、侧栏与 SUMMARY，同步独立 doctest；Rust 9 项基线保持原样，指南增加独立构建门禁，不将 npm 变成 Cargo 消费前提。

已有 action-step.md 保留名称中的空格变更不是本批所作，继续保留；本批没有顺手修正或重新归属它。异机接续前需按用户指令核对并提交全部待交付开发代码与指南，排除编译产物和下载依赖。
