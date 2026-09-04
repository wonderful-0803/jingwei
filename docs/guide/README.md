# Jingwei 开发指南站

基于 VitePress 1.6.4 的独立文档工程。正文在 [src](src/index.md)，构建不依赖 Cargo、框架源码或私有测试目录；发布物是生成的静态文件，不是本目录的 npm 包（private=true）。

## 快速启动

Node.js 24.19.0 / npm 11.17.0。在本目录执行：

```sh
npm ci
npm run docs:dev
```

开发服务只监听本机。Windows 可使用 `npm.cmd`。缓存位于 `target/npm-cache`，已准备好依赖的机器可用 `npm ci --offline`。`.npmrc` 禁用安装期脚本；esbuild 使用锁定的平台二进制。

## 构建与预览

```sh
npm run docs:build
npm run docs:preview
```

- 默认产物：`target/site`，base 为 `/`。
- 项目 Pages：`npm run docs:build:pages`，产物为 `target/pages`，base 为 `/jingwei/`。
- Pages 本地预览：`npm run docs:preview -- --base /jingwei/ --outDir target/pages`。
- 其他子路径：`npm run docs:build -- --base /developer-guide/`。
- 只向静态 HTTP 托管上传选定的输出目录；不要上传源码、依赖、缓存、内部文档或测试。

构建采用 Vite 6.4.3 安全依赖覆盖，并以锁文件固定全部依赖；不要删除 overrides 后重新安装。背景、编写约定和部署说明见[编写与发布指南](src/writing-guide.md)。

## 验证与交付边界

站内死链会导致 VitePress 构建失败。私有验证工程在仓库根目录额外运行以下命令，检查代码块转换、静态资源/锚点、中文与 API 搜索、目录完整性及产物隔离：

```sh
node --test tests/guide-site/markdown.test.mjs
node tests/guide-site/check.mjs
node tests/guide-site/check.mjs docs/guide/target/pages /jingwei/
cargo test --manifest-path tests/model-protocol/Cargo.toml --doc --locked --offline --target-dir target
```

这些检查不是 `docs:build` 的依赖，独立交付指南源码时不需要携带测试工程。现有 Rust 9 项基线保持不变，指南使用独立的构建门禁。仓库的 `.github/workflows/guide.yml` 仅构建、校验和保存静态产物，没有 Pages 写权限或部署步骤；本机验证不等于远端 CI 已执行。

新增页面须同时更新 VitePress 分组侧栏与 `src/SUMMARY.md`。指南 Markdown 保留 rustdoc 隐藏行与代码属性，站点渲染时适配，源文件继续用于独立 doctest。`srcDir` 固定为 `src`，不会把本 README、内部实施记录或根目录 PRD 自动发布出去。
