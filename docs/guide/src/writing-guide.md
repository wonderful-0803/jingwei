# 编写与发布指南

开发指南使用 VitePress 默认文档主题，Markdown 是唯一正文来源。后续新增能力时，在同一个开发变更中更新源码契约、使用教程与独立验证；不要复制第二套站点正文。

## 本地写作

准备 Node.js 24.19.0 和 npm 11（本仓库锁定 npm 11.17.0），进入仓库的 `docs/guide`：

```sh
npm ci
npm run docs:dev
```

终端会显示本机地址，打开后修改 `src` 内的 Markdown 即可即时预览。默认只监听 `127.0.0.1`，不对局域网开放。Windows 若 PowerShell 限制 npm 脚本执行，可将命令中的 `npm` 替换为 `npm.cmd`。

依赖已缓存后可使用 `npm ci --offline`；首次安装需要 npm 官方仓库。`package-lock.json` 纳入版本管理，日常安装不重新选择依赖版本。VitePress 固定为 1.6.4，不跟随 alpha 通道。

构建依赖通过 `overrides` 固定为 Vite 6.4.3：稳定版 VitePress 原声明的 Vite 5 已受安全公告影响，修复版本见 [Vite 官方公告](https://github.com/vitejs/vite/security/advisories/GHSA-fx2h-pf6j-xcff)。这是本项目验证的依赖覆盖，不表示 VitePress 1.x 上游已经修改其声明；升级依赖时必须重跑站点检查。安装默认不执行依赖生命周期脚本，使用锁定依赖提供的平台二进制。

## 新增或修改一篇指南

1. 在 `src` 新建或编辑 Markdown，使用一个一级标题，正文按二、三级标题组织。
2. 先说明这项能力能解决什么、需要什么宿主前提，再给最小教学代码，最后说明错误和未覆盖的边界。
3. 新文章同时加入 `.vitepress/config.mjs` 的分组导航和 `src/SUMMARY.md`，便于网站和仓库阅读。
4. 站内链接使用相对 Markdown 路径，例如 `[任务预算](task-budget.md)`，不要硬编码最终部署域名或 `/jingwei/` 前缀。
5. 公共指南不引用 `docs/internal`、私有测试夹具或原始评测资料。实现记录留在内部文档区，不进入静态站点。
6. 构建检查通过后，再按仓库流程提交。发布指南与发布框架是独立动作。

现有 Rust 教学片段继续供独立工程执行 doctest。保留 `rust,no_run` 等标记以及 `# ` 开头的 rustdoc 隐藏辅助行；站点渲染会将它们转换为正常 Rust 高亮并隐藏辅助行，不修改源 Markdown。新的可编译教程需要在独立工程加入对应验证，不把测试加入指南构建依赖。

::: tip 保持标准 Markdown
优先使用标题、列表、表格、链接和 fenced code；提示信息可使用 VitePress 的 `tip`、`warning` 容器。不要为了装饰加入客户端组件、远程字体、统计脚本或外部搜索服务。
:::

## 构建可发布的静态目录

在 `docs/guide` 执行：

```sh
npm run docs:build
npm run docs:preview
```

默认静态产物是 `docs/guide/target/site`，部署根路径为 `/`。该目录包含 HTML、脚本、样式与本地搜索索引，可以由普通 HTTP 静态服务器提供，不需要 Node.js 后端、Cargo 源码或测试工程。请通过 HTTP 阅读，直接双击 `file://` HTML 不保证路由和搜索可用。

网站保留 `.html` 页面地址，不要求服务器配置无扩展名路由重写。VitePress 会检查站内页面链接，不关闭 `ignoreDeadLinks` 来掩盖断链。搜索在浏览器内运行，中文分词使用现代浏览器的 `Intl.Segmenter`，不把查询发送给外部服务。

## 子路径与以后公开发布

如果将来发布到 GitHub 项目 Pages 的 `/jingwei/` 子路径，使用：

```sh
npm run docs:build:pages
npm run docs:preview -- --base /jingwei/ --outDir target/pages
```

这份产物位于 `docs/guide/target/pages`。其他部署路径可以显式覆盖，例如：

```sh
npm run docs:build -- --base /developer-guide/
```

部署前选择匹配的 base 并重新构建，不直接复制带错误前缀的 HTML。部署对象仅为选定的静态产物目录，不能上传仓库根目录、`docs/internal`、npm 缓存或 `node_modules`。

本轮只准备构建与预览，不开通托管、修改 GitHub Pages 设置或自动发布。以后确认公开发布位置和版本后，再增加部署步骤。VitePress 的[安装说明](https://vuejs.github.io/vitepress/v1/guide/getting-started)、[配置参考](https://vuejs.github.io/vitepress/v1/reference/site-config)和[部署说明](https://vuejs.github.io/vitepress/v1/guide/deploy)可用于维护站点。
