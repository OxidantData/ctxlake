import { defineConfig } from 'vitepress'

// The docs live in ../docs as plain Markdown and are read directly on GitHub. This
// site renders the SAME files — there is no separate copy to drift.
//
// Two house conventions make that work without rewriting anything:
//   - No frontmatter anywhere, so GitHub shows no stray YAML block.
//   - Relative links include the `.md` suffix ([storage.md](storage.md)), which is
//     both the Oxidant docs convention and VitePress's native form.
export default defineConfig({
  title: 'ctxlake',
  description:
    'Zero-compute context lake for agent fleets. Keeps Claude Code, Cursor and Hermes agents in sync through object storage alone.',
  srcDir: '../docs',
  outDir: './.vitepress/dist',
  cleanUrls: true,
  lastUpdated: true,

  // Dark-first, matching oxidantdata.com. Readers can still switch.
  appearance: 'dark',

  head: [
    ['link', { rel: 'preconnect', href: 'https://fonts.googleapis.com' }],
    ['link', { rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: '' }],
    [
      'link',
      {
        rel: 'stylesheet',
        href: 'https://fonts.googleapis.com/css2?family=Geist:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap',
      },
    ],
  ],

  // These docs are read in two places, and a few links deliberately point at repo
  // files that live outside srcDir (AGENTS.md, LICENSE). Those resolve correctly on
  // GitHub but are unreachable from the site, so they are exempted by pattern —
  // narrowly, so that genuine dead links inside docs/ still fail the build.
  // docs/README.md is the index GitHub renders when you open the folder. Map it to the
  // site root too, so one file serves both and docs/ needs no index.md that would sit
  // there looking stray next to the README.
  rewrites: { 'README.md': 'index.md' },

  // VitePress normalizes these to `./../AGENTS` — leading `./`, no extension — so the
  // pattern matches any link that escapes srcDir upward, and nothing inside it.
  ignoreDeadLinks: [/^(\.\/)?\.\.\//],

  markdown: {
    // Mermaid blocks render on GitHub natively; on the site they stay fenced rather
    // than silently disappearing. Swap in a mermaid plugin here if that changes.
    lineNumbers: false,
  },

  themeConfig: {
    siteTitle: 'ctxlake',
    outline: [2, 3],

    nav: [
      { text: 'Get started', link: '/getting-started' },
      { text: 'Architecture', link: '/architecture' },
      { text: 'Scaling', link: '/scaling' },
      { text: 'Oxidant', link: 'https://oxidantdata.com' },
    ],

    sidebar: [
      {
        text: 'Start here',
        items: [
          { text: 'Getting started', link: '/getting-started' },
          { text: 'Adopting it', link: '/adopting' },
          { text: 'Importing history', link: '/import' },
          { text: 'Concepts', link: '/concepts' },
        ],
      },
      {
        text: 'Using it',
        items: [
          { text: 'Coordination', link: '/coordination' },
          { text: 'Summarization', link: '/summarization' },
          { text: 'Memory', link: '/memory' },
          { text: 'MCP tools', link: '/mcp' },
        ],
      },
      {
        text: 'Runtimes',
        items: [
          { text: 'Claude Code', link: '/runtimes/claude-code' },
          { text: 'Cursor', link: '/runtimes/cursor' },
          { text: 'Hermes', link: '/runtimes/hermes' },
        ],
      },
      {
        text: 'Operating it',
        items: [
          { text: 'Storage backends', link: '/storage' },
          { text: 'Bucket layout', link: '/layout' },
          { text: 'Scaling limits', link: '/scaling' },
          { text: 'Security', link: '/security' },
          { text: 'Troubleshooting', link: '/troubleshooting' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'CLI', link: '/cli' },
          { text: 'Configuration', link: '/config' },
          { text: 'Architecture', link: '/architecture' },
        ],
      },
    ],

    socialLinks: [{ icon: 'github', link: 'https://github.com/OxidantData/ctxlake' }],

    search: { provider: 'local' },

    editLink: {
      pattern: 'https://github.com/OxidantData/ctxlake/edit/main/docs/:path',
      text: 'Edit this page on GitHub',
    },

    footer: {
      message: 'AGPL-3.0-or-later, with commercial licenses available. Pre-alpha.',
      copyright: '© 2026 Oxidant Data',
    },
  },
})
