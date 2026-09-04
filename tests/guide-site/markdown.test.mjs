import assert from 'node:assert/strict'
import test from 'node:test'
import { configureMarkdown } from '../../docs/guide/.vitepress/markdown.mjs'

function renderToken(info, content, type = 'fence') {
  let rule
  configureMarkdown({ core: { ruler: { after(stage, name, callback) {
    assert.equal(stage, 'block')
    assert.equal(name, 'jingwei-rustdoc-fences')
    rule = callback
  } } } })
  const token = { info, content, type }
  rule({ tokens: [token] })
  return token
}

test('rustdoc attributes use Rust highlighting without changing source text', () => {
  const source = 'let x = 1;\n# Ok::<(), Error>(())\n'
  const token = renderToken('rust,no_run', source)
  assert.equal(token.info, 'rust')
  assert.equal(token.content, 'let x = 1;\n')
  assert.equal(source, 'let x = 1;\n# Ok::<(), Error>(())\n')
})

test('hidden setup lines disappear while Rust attributes remain visible', () => {
  const token = renderToken('rust', '# use std::sync::Arc;\n#\n#[derive(Clone)]\nstruct Task;\n')
  assert.equal(token.content, '#[derive(Clone)]\nstruct Task;\n')
})

test('escaped hash lines and code-block metadata survive', () => {
  const token = renderToken('rust,ignore,edition2024 title="example.rs"', '## visible\n  ##[attribute]\n')
  assert.equal(token.info, 'rust title="example.rs"')
  assert.equal(token.content, '# visible\n  #[attribute]\n')
})

test('other languages and non-fence tokens are not changed', () => {
  assert.equal(renderToken('sh', '# shell comment\n').content, '# shell comment\n')
  assert.equal(renderToken('rustic', '# not Rust\n').content, '# not Rust\n')
  assert.equal(renderToken('rust', '# text\n', 'paragraph').content, '# text\n')
})
