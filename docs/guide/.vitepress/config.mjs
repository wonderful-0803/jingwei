import { defineConfig } from 'vitepress'
import { configureMarkdown } from './markdown.mjs'

export default defineConfig({
  lang: 'zh-CN',
  title: 'Jingwei',
  titleTemplate: ':title · Jingwei 开发指南',
  description: '面向端侧小模型的 Rust Agent Harness：模型、工具、任务预算与持久运行。',
  srcDir: './src',
  outDir: './target/site',
  cacheDir: './target/cache/vitepress',
  base: '/',
  cleanUrls: false,
  ignoreDeadLinks: false,
  appearance: true,
  lastUpdated: false,
  markdown: {
    config: configureMarkdown,
    theme: { light: 'github-light', dark: 'github-dark' },
  },
  vite: {
    cacheDir: './target/cache/vite',
    server: { host: '127.0.0.1' },
    preview: { host: '127.0.0.1' },
  },
  themeConfig: {
    siteTitle: 'Jingwei 开发指南',
    nav: [
      { text: '指南', link: '/' },
      { text: '目录', link: '/SUMMARY' },
      { text: 'API 参考', link: '/api-reference' },
      { text: 'v0.1 开发中', link: '/development' },
    ],
    sidebar: [
      {
        text: '开始使用',
        items: [
          { text: '概览与当前进度', link: '/' },
          { text: '开发环境与兼容性', link: '/development' },
        ],
      },
      {
        text: '模型与动作',
        items: [
          { text: '结构化模型协议', link: '/model-protocol' },
          { text: '模型调度与超时', link: '/model-scheduling' },
          { text: '单步动作执行', link: '/action-step' },
        ],
      },
      {
        text: '任务与预算',
        items: [
          { text: '任务预算账本', link: '/task-budget' },
          { text: '预算快照与恢复原语', link: '/budget-checkpoints' },
          { text: '文件检查点存储', link: '/file-checkpoint-store' },
          { text: '持久预算运行', link: '/durable-budget' },
          { text: '宿主审计增额', link: '/budget-grants' },
        ],
      },
      {
        text: '参考与贡献',
        items: [
          { text: 'API 参考', link: '/api-reference' },
          { text: '编写与发布指南', link: '/writing-guide' },
        ],
      },
    ],
    outline: { level: [2, 3], label: '本页内容' },
    sidebarMenuLabel: '目录',
    returnToTopLabel: '返回顶部',
    darkModeSwitchLabel: '外观',
    lightModeSwitchTitle: '切换到浅色模式',
    darkModeSwitchTitle: '切换到深色模式',
    skipToContentLabel: '跳转到正文',
    docFooter: { prev: '上一篇', next: '下一篇' },
    editLink: {
      pattern: 'https://github.com/wonderful-0803/jingwei/edit/dev/docs/guide/src/:path',
      text: '在 GitHub 上编辑此页',
    },
    socialLinks: [{ icon: 'github', link: 'https://github.com/wonderful-0803/jingwei' }],
    search: {
      provider: 'local',
      options: {
        // This function is serialized into the browser. No Node APIs or closures.
        miniSearch: {
          options: {
            tokenize: function (text) {
              return Array.from(new Intl.Segmenter('zh-CN', { granularity: 'word' }).segment(text))
                .filter((part) => part.isWordLike)
                .map((part) => part.segment)
            },
          },
          searchOptions: { combineWith: 'AND' },
        },
        translations: {
          button: { buttonText: '搜索文档', buttonAriaLabel: '搜索开发指南' },
          modal: {
            displayDetails: '显示详细列表',
            resetButtonTitle: '清除搜索',
            backButtonTitle: '关闭搜索',
            noResultsText: '没有找到相关内容',
            footer: {
              selectText: '选择',
              selectKeyAriaLabel: '回车键',
              navigateText: '切换',
              navigateUpKeyAriaLabel: '上箭头',
              navigateDownKeyAriaLabel: '下箭头',
              closeText: '关闭',
              closeKeyAriaLabel: 'Esc',
            },
          },
        },
      },
    },
    notFound: {
      title: '页面不存在',
      quote: '这篇指南可能尚未提供，或地址已经改变。',
      linkLabel: '返回开发指南',
      linkText: '返回开发指南',
    },
    footer: { message: 'Jingwei · 小模型 Agent 运行时基础设施 · v0.1 开发中' },
  },
})
