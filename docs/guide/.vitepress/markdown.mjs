// Keep the same Rust snippets useful to rustdoc and to readers. Only presentation
// tokens change; the Markdown files remain untouched for independent doctests.
export function configureMarkdown(md) {
  md.core.ruler.after('block', 'jingwei-rustdoc-fences', (state) => {
    for (const token of state.tokens) {
      if (token.type !== 'fence' || !/^rust(?:[\s,]|$)/.test(token.info)) continue
      token.info = token.info.replace(/^rust(?:,[^\s]+)?/, 'rust')
      token.content = token.content
        .split('\n')
        .filter((line) => !/^\s*#(?: |$)/.test(line))
        .map((line) => line.replace(/^(\s*)##/, '$1#'))
        .join('\n')
    }
  })
}
