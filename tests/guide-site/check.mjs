// Private artifact verification. The guide's npm build does not import this file.
// This reads generated files; it does not start a browser or require a backend.
import assert from 'node:assert/strict'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, relative, resolve, sep } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import config from '../../docs/guide/.vitepress/config.mjs'

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const guide = resolve(repo, 'docs/guide')
const output = resolve(repo, process.argv[2] ?? 'docs/guide/target/site')
const base = process.argv[3] ?? '/'
assert.match(base, /^\/(?:[^?#]*\/)?$/)
assert(existsSync(resolve(output, 'index.html')), 'build the static guide first')

function files(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    assert(!entry.isSymbolicLink(), `unexpected symlink: ${entry.name}`)
    const path = resolve(dir, entry.name)
    return entry.isDirectory() ? files(path) : [path]
  })
}

const sourcePages = files(resolve(guide, 'src')).filter((path) => path.endsWith('.md'))
const expectedPages = sourcePages.map((path) => relative(resolve(guide, 'src'), path).replace(/\.md$/, '.html')).sort()
const generated = files(output)
const pages = generated.filter((path) => path.endsWith('.html'))
assert.deepEqual(pages.map((path) => relative(output, path)).filter((name) => name !== '404.html').sort(), expectedPages)
for (const path of generated) {
  assert(!/(^|\/)(?:node_modules|internal|tests|crates|\.git|\.vitepress)(\/|$)/.test(relative(output, path).split(sep).join('/')))
  assert(!/\.(?:rs|md|toml|map)$/.test(path), `source or private file in public output: ${path}`)
}

const html = new Map(pages.map((path) => [path, readFileSync(path, 'utf8')]))
let linksChecked = 0
for (const [path, content] of html) {
  assert(content.includes('lang="zh-CN"'), `missing locale: ${path}`)
  assert(content.includes('Jingwei'), `missing site title: ${path}`)
  assert(!/data-lang="rust,/.test(content), `unhandled rustdoc fence: ${path}`)
  for (const [, value] of content.matchAll(/\b(?:href|src)="([^"]+)"/g)) {
    if (/^(?:https?:|data:|mailto:|tel:|blob:|\/\/)/.test(value)) continue
    const url = new URL(value.replaceAll('&amp;', '&'), `https://guide.invalid${base}${relative(output, path).split(sep).join('/')}`)
    assert(url.pathname.startsWith(base), `base escaped by ${value} in ${path}`)
    const requested = decodeURIComponent(url.pathname.slice(base.length))
    const target = resolve(output, requested.endsWith('/') || !requested ? `${requested}index.html` : requested)
    const within = relative(output, target)
    assert(within !== '..' && !within.startsWith(`..${sep}`), 'link escapes public output')
    assert(existsSync(target), `missing static target: ${value} in ${path}`)
    if (url.hash && html.has(target)) {
      const anchor = decodeURIComponent(url.hash.slice(1))
      const ids = new Set(Array.from(html.get(target).matchAll(/\bid="([^"]+)"/g), (match) => match[1]))
      assert(ids.has(anchor), `missing anchor ${url.hash} in ${target}`)
    }
    linksChecked++
  }
}

const overview = html.get(resolve(output, 'index.html'))
assert(overview.includes('搜索文档') && overview.includes('本页内容'))
assert(overview.includes('VPNavBarAppearance') && overview.includes('VPSidebar'))
const protocol = html.get(resolve(output, 'model-protocol.html'))
assert(protocol.includes('class="copy"'), 'code copy control missing')
for (const [, block] of protocol.matchAll(/<code>([\s\S]*?)<\/code>/g)) {
  assert(!/# Ok::/.test(block.replace(/<[^>]+>/g, '')), 'rustdoc scaffolding leaked into displayed code')
}

// Check the actual generated local index with the same MiniSearch options as
// VitePress, including Chinese tokenization. No external search service is used.
const indexFiles = generated.filter((path) => /@localSearchIndexroot\.[^/\\]+\.js$/.test(path))
assert.equal(indexFiles.length, 1)
const indexJson = (await import(pathToFileURL(indexFiles[0]).href)).default
const require = createRequire(resolve(guide, 'package.json'))
const { default: MiniSearch } = await import(pathToFileURL(require.resolve('minisearch')).href)
const options = config.themeConfig.search.options.miniSearch
const index = MiniSearch.loadJSON(indexJson, {
  fields: ['title', 'titles', 'text'],
  storeFields: ['title', 'titles'],
  ...options.options,
  searchOptions: { prefix: true, fuzzy: 0.2, ...options.searchOptions },
})
for (const [query, page] of [['预算', 'task-budget.html'], ['增额', 'budget-grants.html'], ['BudgetExecutionLease', 'durable-budget.html']]) {
  assert(index.search(query).some((result) => result.id.includes(page)), `search cannot find ${page} for ${query}`)
}
assert(!indexJson.includes('/internal/'), 'private documents were indexed')

const sidebarLinks = new Set(config.themeConfig.sidebar.flatMap((section) => section.items.map((item) => item.link)))
const summary = readFileSync(resolve(guide, 'src/SUMMARY.md'), 'utf8')
for (const page of expectedPages.filter((page) => page !== 'SUMMARY.html')) {
  const route = page === 'index.html' ? '/' : `/${page.slice(0, -5)}`
  assert(sidebarLinks.has(route), `page missing from sidebar: ${page}`)
  assert(summary.includes(`](${page.replace(/\.html$/, '.md')})`), `page missing from Markdown directory: ${page}`)
}

console.log(JSON.stringify({ base, pages: pages.length, linksChecked, searchQueries: 3, publicSourceFiles: 0 }, null, 2))
