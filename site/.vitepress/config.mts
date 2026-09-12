import { defineConfig } from 'vitepress'
import { withMermaid } from 'vitepress-plugin-mermaid'

// The docs live in ../docs as plain Markdown and are read directly on GitHub. This
// site renders the SAME files — there is no separate copy to drift.
//
// Two house conventions make that work without rewriting anything:
//   - No frontmatter anywhere, so GitHub shows no stray YAML block.
//   - Relative link TARGETS include the `.md` suffix ([Storage](storage.md)), which is
//     both the Oxidant docs convention and VitePress's native form. The link *text* is
//     the page's title, not its filename — `[storage.md](storage.md)` reads naturally
//     on GitHub and like a leaked implementation detail on a docs site.
export default withMermaid(
  defineConfig({
  title: 'ctxlake',
  description:
    'Zero-compute context lake for agent fleets. Keeps Claude Code, Cursor and Hermes agents in sync through object storage alone.',
  srcDir: '../docs',
  outDir: './.vitepress/dist',
  // Extension-ful URLs (/getting-started.html). cleanUrls would emit /getting-started,
  // which a plain S3 origin cannot resolve without a CloudFront Function to append
  // .html — prettier links are not worth an edge function in the serving path.
  cleanUrls: false,
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
    lineNumbers: false,
  },

  // Mermaid renders natively on GitHub but not in VitePress, so ```mermaid fences
  // were shipping to the site as literal source. withMermaid() adds the renderer;
  // the theme below keeps diagrams legible against both light and dark pages
  // without a second palette to maintain.
  mermaid: {
    theme: 'base',
    themeVariables: {
      fontFamily: 'Geist, Inter, ui-sans-serif, system-ui, sans-serif',
      primaryColor: '#f5f5f5',
      primaryTextColor: '#0a0a0a',
      primaryBorderColor: '#a3a3a3',
      lineColor: '#737373',
      secondaryColor: '#e5e5e5',
      tertiaryColor: '#fafafa',
    },
  },

  themeConfig: {
    siteTitle: 'ctxlake',
    outline: [2, 3],

    nav: [
      { text: 'Get started', link: '/getting-started' },
      { text: 'Architecture', link: '/architecture' },
      { text: 'Reference', link: '/reference' },
      { text: 'Oxidant', link: 'https://oxidantdata.com' },
    ],

    sidebar: [
      {
        text: 'Start here',
        items: [
          { text: 'Getting started', link: '/getting-started' },
          { text: 'How it works', link: '/how-it-works' },
          { text: 'Adding it', link: '/adding-it' },
        ],
      },
      {
        text: 'Using it',
        items: [
          { text: 'Memory', link: '/memory' },
          { text: 'Runtimes', link: '/runtimes' },
          { text: 'Storage', link: '/storage' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'CLI and config', link: '/reference' },
          { text: 'Architecture', link: '/architecture' },
          { text: 'Security', link: '/security' },
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
  }),
)
