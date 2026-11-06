# Vendored front-end libraries

Copied unmodified from the npm registry tarballs; trunk copies this directory whole.

| Path | Library | Version | Source |
|---|---|---|---|
| `highlight.js` | highlight.js | 11.10.0 | highlightjs.org build (ES module) |
| `katex/katex.min.js`, `katex/katex.min.css`, `katex/fonts/*.woff2` | KaTeX | 0.18.9 | https://registry.npmjs.org/katex/-/katex-0.18.9.tgz (`dist/`, woff2 only) |
| `mermaid/mermaid.min.js` | Mermaid | 12.0.0 | https://registry.npmjs.org/mermaid/-/mermaid-12.0.0.tgz (`dist/mermaid.min.js`, sets `globalThis.mermaid`) |
| `dompurify/purify.min.js`, `dompurify/LICENSE` | DOMPurify | 3.4.16 | https://registry.npmjs.org/dompurify/-/dompurify-3.4.16.tgz (`dist/purify.min.js`, UMD, sets `globalThis.DOMPurify`); the allowlist sanitiser for Mermaid's SVG (Mermaid bundles its own copy but does not expose it) |

KaTeX, Mermaid and DOMPurify are loaded lazily by `assets/codeblocks.js` on first use.
