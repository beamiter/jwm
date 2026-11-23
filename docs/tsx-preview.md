# TSX preview for JWM Notes

This first version is an **external-browser development tool**, not an embedded
Notes panel and not a sandbox. Edit and save in JWM Notes, then view the saved
project in your browser. Nothing starts just because a TSX file is opened.

## Start and stop

Requirements: Linux, Node 22.12+ (or 20.19+), and an existing trusted project with
Vite 7/8, React and React DOM already installed inside its directory. The launcher
never installs dependencies. Monorepos with dependencies outside the selected
project and global dependency fallback are deliberately unsupported.

```sh
bash scripts/install-tsx-preview.sh
~/.local/bin/jwm-tsx-preview --project /path/to/project --check
~/.local/bin/jwm-tsx-preview --project /path/to/project --trust-project --open
```

`--check` only validates files/package metadata. It does not import Vite, run
project code, install packages or start a server. `--trust-project` explicitly
acknowledges execution of the project's local tooling dependencies with your user
permissions. Only use reviewed projects and dependencies. `--open` is optional:
without it, copy the printed URL into a browser. Ctrl+C stops the preview and
removes its temporary dependency cache. No daemon or auto-start entry is added.

The default entry is the project's `index.html`. A single default-exported React
component can instead be mounted without props:

```sh
~/.local/bin/jwm-tsx-preview --project /path/to/project \
  --component src/App.tsx --trust-project --open
```

Use `--port 5173` for a fixed port; an occupied port fails rather than silently
switching. By default an available random loopback port is selected. Save edits
to update the preview. Syntax errors appear in Vite's browser overlay. In
component mode, a module initialization error, render failure or missing default
export shows a message instead of a blank page. Fix the source and save to retry. This does not provide
React Fast Refresh state preservation or TypeScript type checking.

## Compatibility and security boundaries

- Uses a fixed Vite configuration. It does not execute package scripts, load
  Vite/PostCSS configuration, load `.env` files, expose `VITE_*` environment
  variables, or copy/serve Vite's `public/` directory as public assets.
- Vite's TypeScript compiler can still read `tsconfig` files and their extensions.
  Project source imports and local tooling dependencies are trusted executable
  inputs. This is **not** a filesystem or process sandbox. Use a separately
  isolated environment for untrusted repositories instead of this tool.
- Custom Vite plugins, aliases, framework adapters, SSR and config-dependent CSS
  processing are not supported. Basic React TSX/JSX, CSS and normal module imports
  work. Dependencies must resolve inside the project, including symlink targets.
- The HTTP server binds only `127.0.0.1`. HTTP requests require its exact loopback
  Host and, when supplied, matching Origin. Traversal, external symlinks, dotfiles
  (except the dependency manager's `node_modules/.pnpm` paths), private-key file
  suffixes and arbitrary Vite internal module IDs are rejected. The project and
  temporary cache are the only configured Vite filesystem roots. These are
  defense-in-depth checks, **not a guarantee that project files are secrets**.
- Vite handles WebSocket upgrades with its own token/host safeguards; the custom
  HTTP middleware does not cover that protocol. Other local processes can reach
  the server. Stop it when finished.
- Previewed JavaScript runs in your browser and can make network requests. The
  launcher does not promise network isolation, a restrictive CSP, or protection
  against hostile project/dependency code. Browser extensions and browser state
  are outside its protection. No server is deliberately exposed to the LAN.
- Edits are read from disk, not unsaved editor buffers. Component preview requires
  a default export that can render without application-specific providers/props.

## Verification

```sh
node --test scripts/test-tsx-preview.mjs
npm ci --ignore-scripts --prefix tests/tsx-preview
cd tests/tsx-preview && npx playwright install chromium && cd ../..
node scripts/test-tsx-preview-e2e.mjs
```

These dependency/browser installation commands are for explicitly running this
repository's tests only. The runtime launcher never performs them. Set
`CHROMIUM_PATH` to use an already installed browser. CI exercises a real React
project and component, clicks, CSS, saved-file updates, syntax-error recovery,
request/config/environment boundaries, and shutdown/cache cleanup.

Implementation references: [Vite JavaScript API](https://vite.dev/guide/api-javascript),
[server options](https://vite.dev/config/server-options), and
[shared options](https://vite.dev/config/shared-options).
